package identity

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"time"
)

// KeySource resolves a kid to an ES256 public key.
type KeySource interface {
	Key(ctx context.Context, kid string) (*ecdsa.PublicKey, error)
}

// ErrUnknownKid is returned when the key set has no key with the kid.
var ErrUnknownKid = errors.New("unknown kid")

// JWK is one key of an RFC 7517 key set (identity/v1 publishes EC P-256 keys only).
type JWK struct {
	Kty string `json:"kty"`
	Crv string `json:"crv"`
	X   string `json:"x"`
	Y   string `json:"y"`
	Kid string `json:"kid"`
	Alg string `json:"alg"`
	Use string `json:"use"`
}

// ParseJWKS reads a key set; keys that are not EC P-256 ES256 signing keys are skipped.
func ParseJWKS(raw []byte) (map[string]*ecdsa.PublicKey, error) {
	var set struct {
		Keys []JWK `json:"keys"`
	}
	if err := json.Unmarshal(raw, &set); err != nil {
		return nil, fmt.Errorf("key set is not JSON: %w", err)
	}
	keys := map[string]*ecdsa.PublicKey{}
	for _, k := range set.Keys {
		if k.Kty != "EC" || k.Crv != "P-256" || (k.Alg != "" && k.Alg != "ES256") || (k.Use != "" && k.Use != "sig") || k.Kid == "" {
			continue
		}
		x, err := base64.RawURLEncoding.DecodeString(k.X)
		if err != nil || len(x) != 32 {
			return nil, fmt.Errorf("key %q: x is not a 32-byte base64url value", k.Kid)
		}
		y, err := base64.RawURLEncoding.DecodeString(k.Y)
		if err != nil || len(y) != 32 {
			return nil, fmt.Errorf("key %q: y is not a 32-byte base64url value", k.Kid)
		}
		pub, err := ecdsa.ParseUncompressedPublicKey(elliptic.P256(), append(append([]byte{4}, x...), y...))
		if err != nil {
			return nil, fmt.Errorf("key %q: not a P-256 point: %w", k.Kid, err)
		}
		keys[k.Kid] = pub
	}
	if len(keys) == 0 {
		return nil, errors.New("key set has no EC P-256 ES256 signing key")
	}
	return keys, nil
}

// StaticKeys is a fixed key set (tests, and a set loaded once).
type StaticKeys map[string]*ecdsa.PublicKey

// Key implements KeySource.
func (s StaticKeys) Key(_ context.Context, kid string) (*ecdsa.PublicKey, error) {
	if k, ok := s[kid]; ok {
		return k, nil
	}
	return nil, ErrUnknownKid
}

// RemoteKeys fetches the issuer's key set from URL and caches it (contract: Cache-Control max-age, default 600 s; an
// unknown kid refetches at most once per 30 s). When a refetch fails the cached keys keep working: NewAPI being down
// stops new tokens, not verification.
type RemoteKeys struct {
	URL    string
	Client *http.Client

	mu        sync.Mutex
	keys      map[string]*ecdsa.PublicKey
	fresh     time.Time // keys are fresh until then
	lastFetch time.Time
}

// RefetchInterval is the least time between two fetches caused by an unknown kid.
const RefetchInterval = 30 * time.Second

// Key implements KeySource.
func (r *RemoteKeys) Key(ctx context.Context, kid string) (*ecdsa.PublicKey, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	now := time.Now()
	if k, ok := r.keys[kid]; ok && now.Before(r.fresh) {
		return k, nil
	}
	if r.keys == nil || now.After(r.fresh) || now.Sub(r.lastFetch) >= RefetchInterval {
		if err := r.fetch(ctx, now); err != nil {
			if k, ok := r.keys[kid]; ok {
				return k, nil
			}
			return nil, err
		}
	}
	if k, ok := r.keys[kid]; ok {
		return k, nil
	}
	return nil, ErrUnknownKid
}

func (r *RemoteKeys) fetch(ctx context.Context, now time.Time) error {
	r.lastFetch = now
	client := r.Client
	if client == nil {
		client = &http.Client{Timeout: 15 * time.Second}
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, r.URL, nil)
	if err != nil {
		return fmt.Errorf("identity keys: GET %s: %w", r.URL, err)
	}
	resp, err := client.Do(req)
	if err != nil {
		return fmt.Errorf("identity keys: GET %s: %w (retry later; cached keys, if any, stay in use)", r.URL, err)
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return fmt.Errorf("identity keys: GET %s: reading the body: %w", r.URL, err)
	}
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("identity keys: GET %s answered %d: %s", r.URL, resp.StatusCode, clip(string(body), 300))
	}
	keys, err := ParseJWKS(body)
	if err != nil {
		return fmt.Errorf("identity keys: GET %s: %w", r.URL, err)
	}
	r.keys = keys
	r.fresh = now.Add(maxAge(resp.Header.Get("Cache-Control")))
	return nil
}

func maxAge(cc string) time.Duration {
	for _, part := range strings.Split(cc, ",") {
		part = strings.TrimSpace(part)
		if v, ok := strings.CutPrefix(part, "max-age="); ok {
			if n, err := strconv.Atoi(v); err == nil && n > 0 {
				return time.Duration(n) * time.Second
			}
		}
	}
	return 600 * time.Second
}

func clip(s string, n int) string {
	s = strings.TrimSpace(s)
	if len(s) > n {
		return s[:n] + "…"
	}
	return s
}
