// Package identity verifies and requests identity/v1 tokens (agentrouter-newapi contracts/identity/v1): ES256 JWTs,
// header typ at+jwt with a kid, verified locally against the issuer's cached key set, never by calling NewAPI.
//
// The verifier follows the contract's "Verifying" section in this order: alg, typ, kid, signature, claims (the shape
// of claims.schema.json), iss, aud, exp and iat (60 s leeway), the lifetime caps, then the scope. A failure is an
// *Error whose Check names the step, as the contract's vectors (tokens.json `error`) do.
package identity

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"regexp"
	"strings"
	"time"

	"github.com/golang-jwt/jwt/v5"
)

// Leeway is the clock leeway for exp and iat (contract: 60 s).
const Leeway = 60 * time.Second

// Lifetime caps (contract "Lifetimes"): any token at most 1 h, a run token (run present) at most 15 min.
const (
	MaxLifetime    = 3600
	MaxRunLifetime = 900
)

// Claims is the payload of an identity/v1 token.
type Claims struct {
	Iss string   `json:"iss"`
	Sub string   `json:"sub"`
	Ten string   `json:"ten"`
	Own string   `json:"own"`
	Pay string   `json:"pay"`
	Aud string   `json:"aud"`
	Scp []string `json:"scp"`
	Run string   `json:"run,omitempty"`
	Iat int64    `json:"iat"`
	Exp int64    `json:"exp"`
	Jti string   `json:"jti"`
}

// Error is a failed verification: Check is the step that failed (alg, typ, kid, signature, claims, iss, aud, exp,
// iat, lifetime, scope, or a caller's own check such as own or age), Detail says why in full.
type Error struct {
	Check  string
	Detail string
	Err    error
}

func (e *Error) Error() string {
	msg := "identity/v1 token rejected (" + e.Check + "): " + e.Detail
	if e.Err != nil {
		msg += ": " + e.Err.Error()
	}
	return msg
}

func (e *Error) Unwrap() error { return e.Err }

func reject(check, format string, args ...any) *Error {
	return &Error{Check: check, Detail: fmt.Sprintf(format, args...)}
}

// Check returns the failed step of a verification error, or "" when err is not one.
func Check(err error) string {
	var e *Error
	if errors.As(err, &e) {
		return e.Check
	}
	return ""
}

// Verifier checks tokens of one issuer.
type Verifier struct {
	// Issuer is compared exactly with iss.
	Issuer string
	// Keys resolves a kid to the issuer's public key.
	Keys KeySource
	// Now is the clock; nil means time.Now.
	Now func() time.Time
}

// Verify checks token for audience aud and, when scope is not empty, that its scp covers scope.
func (v *Verifier) Verify(ctx context.Context, token, aud, scope string) (*Claims, error) {
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		return nil, reject("claims", "not a compact JWS (%d parts, want 3)", len(parts))
	}
	var header struct {
		Alg string `json:"alg"`
		Typ string `json:"typ"`
		Kid string `json:"kid"`
	}
	if err := decodeSegment(parts[0], &header); err != nil {
		return nil, &Error{Check: "alg", Detail: "header is not base64url JSON", Err: err}
	}
	if header.Alg != "ES256" {
		return nil, reject("alg", "alg %q, only ES256 is accepted", header.Alg)
	}
	if header.Typ != "at+jwt" {
		return nil, reject("typ", "typ %q, want at+jwt", header.Typ)
	}
	if header.Kid == "" {
		return nil, reject("kid", "header has no kid")
	}
	key, err := v.Keys.Key(ctx, header.Kid)
	if err != nil {
		return nil, &Error{Check: "kid", Detail: fmt.Sprintf("kid %q is not in the issuer's key set", header.Kid), Err: err}
	}
	sig, err := base64.RawURLEncoding.DecodeString(parts[2])
	if err != nil {
		return nil, &Error{Check: "signature", Detail: "signature is not base64url", Err: err}
	}
	if err := jwt.SigningMethodES256.Verify(parts[0]+"."+parts[1], sig, key); err != nil {
		return nil, &Error{Check: "signature", Detail: fmt.Sprintf("signature does not verify with key %q", header.Kid), Err: err}
	}

	c, err := parseClaims(parts[1])
	if err != nil {
		return nil, err
	}
	if c.Iss != v.Issuer {
		return nil, reject("iss", "iss %q, want %q", c.Iss, v.Issuer)
	}
	if c.Aud != aud {
		return nil, reject("aud", "aud %q, want %q", c.Aud, aud)
	}
	now := time.Now()
	if v.Now != nil {
		now = v.Now()
	}
	if now.After(time.Unix(c.Exp, 0).Add(Leeway)) {
		return nil, reject("exp", "expired at %s (now %s, leeway %s)", time.Unix(c.Exp, 0).UTC().Format(time.RFC3339), now.UTC().Format(time.RFC3339), Leeway)
	}
	if time.Unix(c.Iat, 0).After(now.Add(Leeway)) {
		return nil, reject("iat", "issued at %s, in the future beyond the %s leeway (now %s)", time.Unix(c.Iat, 0).UTC().Format(time.RFC3339), Leeway, now.UTC().Format(time.RFC3339))
	}
	life := c.Exp - c.Iat
	if life > MaxLifetime {
		return nil, reject("lifetime", "lives %d s, at most %d s", life, MaxLifetime)
	}
	if c.Run != "" && life > MaxRunLifetime {
		return nil, reject("lifetime", "run token lives %d s, at most %d s", life, MaxRunLifetime)
	}
	if scope != "" && !Covers(c.Scp, scope) {
		return nil, reject("scope", "scp %v does not cover %s", c.Scp, scope)
	}
	return c, nil
}

func decodeSegment(seg string, into any) error {
	raw, err := base64.RawURLEncoding.DecodeString(seg)
	if err != nil {
		return err
	}
	return json.Unmarshal(raw, into)
}

var (
	subPattern     = regexp.MustCompile(`^(acc_[0-9A-Za-z]{1,64}|svc_[a-z][a-z0-9-]{1,31})$`)
	accountPattern = regexp.MustCompile(`^acc_[0-9A-Za-z]{1,64}$`)
	scopePattern   = regexp.MustCompile(`^[a-z][a-z0-9-]{0,31}:(\*|[0-9A-Za-z][0-9A-Za-z_.-]{0,127}):[a-z][a-z0-9-]{0,31}$`)
	runPattern     = regexp.MustCompile(`^cnv_[0-9A-Za-z_-]{1,64}$`)
	uuidPattern    = regexp.MustCompile(`^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$`)
	audiences      = map[string]bool{"agentspace": true, "agentevents": true, "agentcompute": true, "agenthost": true, "netbird": true}
)

// parseClaims decodes the payload and checks it against the shape of claims.schema.json (required claims, types,
// patterns); unknown claims are ignored (C2).
func parseClaims(seg string) (*Claims, error) {
	raw, err := base64.RawURLEncoding.DecodeString(seg)
	if err != nil {
		return nil, &Error{Check: "claims", Detail: "payload is not base64url", Err: err}
	}
	var m map[string]json.RawMessage
	if err := json.Unmarshal(raw, &m); err != nil {
		return nil, &Error{Check: "claims", Detail: "payload is not a JSON object", Err: err}
	}
	for _, k := range []string{"iss", "sub", "ten", "own", "pay", "aud", "scp", "exp", "iat", "jti"} {
		if _, ok := m[k]; !ok {
			return nil, reject("claims", "claim %q is missing", k)
		}
	}
	for _, k := range []string{"iat", "exp"} {
		var n json.Number
		dec := json.NewDecoder(bytes.NewReader(m[k]))
		dec.UseNumber()
		if err := dec.Decode(&n); err != nil {
			return nil, reject("claims", "claim %q is not a number", k)
		}
		if i, err := n.Int64(); err != nil || i < 0 {
			return nil, reject("claims", "claim %q is not a non-negative integer (%s)", k, n)
		}
	}
	var c Claims
	if err := json.Unmarshal(raw, &c); err != nil {
		return nil, &Error{Check: "claims", Detail: "claims have the wrong types", Err: err}
	}
	switch {
	case c.Iss == "":
		return nil, reject("claims", "iss is empty")
	case !subPattern.MatchString(c.Sub):
		return nil, reject("claims", "sub %q is neither acc_<id> nor svc_<name>", c.Sub)
	case !accountPattern.MatchString(c.Ten), !accountPattern.MatchString(c.Own), !accountPattern.MatchString(c.Pay):
		return nil, reject("claims", "ten/own/pay must be acc_<id> (ten %q, own %q, pay %q)", c.Ten, c.Own, c.Pay)
	case !audiences[c.Aud]:
		return nil, reject("claims", "aud %q is not a layer", c.Aud)
	case len(c.Scp) < 1 || len(c.Scp) > 64:
		return nil, reject("claims", "scp has %d entries (1–64)", len(c.Scp))
	case c.Run != "" && !runPattern.MatchString(c.Run):
		return nil, reject("claims", "run %q is not cnv_<id>", c.Run)
	case !uuidPattern.MatchString(c.Jti):
		return nil, reject("claims", "jti %q is not a lowercase UUID", c.Jti)
	}
	if _, ok := m["run"]; ok && c.Run == "" {
		return nil, reject("claims", "run is present but empty")
	}
	seen := map[string]bool{}
	for _, s := range c.Scp {
		if len(s) > 200 || !scopePattern.MatchString(s) {
			return nil, reject("claims", "scp entry %q is not <kind>:<id>:<right>", s)
		}
		if seen[s] {
			return nil, reject("claims", "scp entry %q appears twice", s)
		}
		seen[s] = true
	}
	return &c, nil
}

// Covers reports whether the held scp entries cover want (contract "Coverage"): the same kind; the held id is the
// same or `*` (and `*` never covers `new`); the held right is the same, or rw where ro is wanted.
func Covers(held []string, want string) bool {
	wk, wid, wr, ok := splitScope(want)
	if !ok {
		return false
	}
	for _, h := range held {
		hk, hid, hr, ok := splitScope(h)
		if !ok || hk != wk {
			continue
		}
		if hid != wid && (hid != "*" || wid == "new") {
			continue
		}
		if hr == wr || (hr == "rw" && wr == "ro") {
			return true
		}
	}
	return false
}

func splitScope(s string) (kind, id, right string, ok bool) {
	p := strings.Split(s, ":")
	if len(p) != 3 || p[0] == "" || p[1] == "" || p[2] == "" {
		return "", "", "", false
	}
	return p[0], p[1], p[2], true
}
