package identity

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"time"

	"golang.org/x/oauth2"
)

// Device login (RFC 8628). The device logs the person in once with a device code and keeps only the newest refresh
// token; short tokens per audience come from it. Two logins, chosen by configuration (Endpoints.ExchangeURL):
//
//   - ZITADEL (AgentIdentity identity/v1 README "Device login (RFC 8628, at ZITADEL)"): the device code and the
//     refresh token are ZITADEL's; every short token is an exchange of ZITADEL's access token at the STS
//     (POST /v1/identity/exchange), so the identity/v1 token's iss is the STS.
//   - NewAPI (the identity plugin's /v1/identity/device/*, legacy): every short token is a refresh at the issuer with
//     the audience and scope. Deleted once the issuer switch has moved every environment to AgentIdentity
//     (agentrouter-newapi#168); nothing new goes into this path.
//
// The device never calls POST /v1/identity/token.

// ClientID is the NewAPI device login's client (legacy). ZITADEL's is the generated client id of the agentdevice
// application, from the configuration.
const ClientID = "agentdevice"

// Audiences and the scope the device asks for in each (the agentdevice application's ceilings at the STS).
const (
	AudienceCompute = "agentcompute"
	AudienceNetBird = "netbird"
)

// ScopeFor is the scope requested for an audience.
func ScopeFor(aud string) []string {
	switch aud {
	case AudienceCompute:
		return []string{"device:*:enrol", "computer:*:report", "computer:*:manage"}
	case AudienceNetBird:
		return []string{"computer:*:ssh"}
	}
	return nil
}

// OAuthError is an OAuth error body (RFC 6749 §5.2; the STS adds retryable and next, R22). The caller's message
// names the component and endpoint.
type OAuthError struct {
	Status      int    `json:"-"`
	Code        string `json:"error"`
	Description string `json:"error_description"`
	Retryable   bool   `json:"retryable"`
	Next        string `json:"next"`
}

func (e *OAuthError) Error() string {
	msg := fmt.Sprintf("answered %d %s: %s", e.Status, e.Code, e.Description)
	if e.Retryable {
		msg += " (retryable)"
	}
	if e.Next != "" {
		msg += "; next: " + e.Next
	}
	return msg
}

// ErrDeviceRemoved: the refresh token was refused for good (the person removed the device, the client id was
// disabled, the account was disabled, or the token was presented twice and the device logged out). The device must
// log in again.
var ErrDeviceRemoved = errors.New("identity/v1: this device's login was removed or revoked")

// ErrNotLoggedIn: there is no refresh token.
var ErrNotLoggedIn = errors.New("identity/v1: this device is not logged in (run `agentdevice login`)")

// Endpoints of the device login.
type Endpoints struct {
	DeviceAuthURL string
	TokenURL      string
	ClientID      string
	// Scope is the device authorization's scope (ZITADEL: openid, offline_access and the project audience).
	Scope []string
	// ExchangeURL selects the ZITADEL login: every short token is an exchange of ZITADEL's access token here (the
	// STS). Empty = the NewAPI device login.
	ExchangeURL string
	// RevokeURL is where the refresh token is revoked when the computer is unbound (RFC 7009; ZITADEL only).
	RevokeURL string
	// LoginPage is the page the person confirms the code on; empty = the answer's verification_uri.
	LoginPage string
}

// zitadel says whether this is the ZITADEL login.
func (e Endpoints) zitadel() bool { return e.ExchangeURL != "" }

// EndpointsFor are the NewAPI device login's endpoints under issuer (legacy, until the issuer switch).
func EndpointsFor(issuer string) Endpoints {
	base := strings.TrimSuffix(issuer, "/")
	return Endpoints{DeviceAuthURL: base + "/v1/identity/device/code", TokenURL: base + "/v1/identity/device/token", ClientID: ClientID}
}

// ZitadelEndpoints are the ZITADEL login's endpoints: RFC 8628 at the ZITADEL instance zitadel for the application
// clientID of project projectID, then exchange at exchangeURL (the STS's POST /v1/identity/exchange). The person
// confirms on Login v2's device page; the answer's verification_uri (<zitadel>/device) is ZITADEL's older page.
func ZitadelEndpoints(zitadel, projectID, clientID, exchangeURL string) Endpoints {
	base := strings.TrimSuffix(zitadel, "/")
	return Endpoints{
		DeviceAuthURL: base + "/oauth/v2/device_authorization",
		TokenURL:      base + "/oauth/v2/token",
		RevokeURL:     base + "/oauth/v2/revoke",
		LoginPage:     base + "/ui/v2/login/device",
		ClientID:      clientID,
		Scope:         []string{"openid", "offline_access", "urn:zitadel:iam:org:project:id:" + projectID + ":aud"},
		ExchangeURL:   exchangeURL,
	}
}

// Lifetimes the device asks the STS for (capped there by the ZITADEL token's exp): an hour for compute/v1 calls;
// a netbird token is minted right before a login and NetBird's SSH server refuses one older than 600 s anyway.
var exchangeLifetime = map[string]int{AudienceCompute: 3600, AudienceNetBird: 600}

// Prompt is what the person must see to confirm the login in their browser.
type Prompt struct {
	UserCode                string
	VerificationURI         string
	VerificationURIComplete string
	Expiry                  time.Time
}

// Grant is a token answer of the device token endpoint.
type Grant struct {
	AccessToken  string `json:"access_token"`
	ExpiresIn    int    `json:"expires_in"`
	Scope        string `json:"scope"`
	Audience     string `json:"audience"`
	RefreshToken string `json:"refresh_token"`
	DeviceID     string `json:"device_id"`
	expiry       time.Time
}

// RefreshStore keeps the newest refresh token (owner-only file).
type RefreshStore interface {
	LoadRefresh() (string, error)
	SaveRefresh(string) error
}

// Session is a logged-in device: it hands out short tokens per audience, refreshing one at a time.
type Session struct {
	Endpoints Endpoints
	Store     RefreshStore
	HTTP      *http.Client

	mu    sync.Mutex
	cache map[string]*Grant
	// subject is ZITADEL's current access token (ZITADEL login only), the parent of every exchange; memory only.
	subject *Grant
}

func (s *Session) client() *http.Client {
	if s.HTTP != nil {
		return s.HTTP
	}
	return &http.Client{Timeout: 30 * time.Second}
}

// Login starts the device login (device_name shown to the person by the NewAPI login), calls show, polls until the
// person decides, and stores the refresh token before it returns. The grant's access token is an agentcompute token
// (ZITADEL login: already exchanged at the STS). RFC 8628 answers (authorization_pending, slow_down, access_denied,
// expired_token) are handled by golang.org/x/oauth2's DeviceAccessToken.
func (s *Session) Login(ctx context.Context, deviceName string, show func(Prompt)) (*Grant, error) {
	ep := s.Endpoints
	cfg := &oauth2.Config{
		ClientID: ep.ClientID,
		Endpoint: oauth2.Endpoint{DeviceAuthURL: ep.DeviceAuthURL, TokenURL: ep.TokenURL, AuthStyle: oauth2.AuthStyleInParams},
		Scopes:   ep.Scope,
	}
	ctx = context.WithValue(ctx, oauth2.HTTPClient, s.client())
	var startOpts, pollOpts []oauth2.AuthCodeOption
	if !ep.zitadel() {
		startOpts = append(startOpts, oauth2.SetAuthURLParam("device_name", deviceName))
		pollOpts = append(pollOpts, oauth2.SetAuthURLParam("audience", AudienceCompute),
			oauth2.SetAuthURLParam("scope", strings.Join(ScopeFor(AudienceCompute), " ")))
	}
	da, err := cfg.DeviceAuth(ctx, startOpts...)
	if err != nil {
		return nil, fmt.Errorf("device login: POST %s: %w", ep.DeviceAuthURL, oauthErr(err))
	}
	p := Prompt{UserCode: da.UserCode, VerificationURI: da.VerificationURI, VerificationURIComplete: da.VerificationURIComplete, Expiry: da.Expiry}
	if ep.LoginPage != "" {
		p.VerificationURI = ep.LoginPage
		p.VerificationURIComplete = ep.LoginPage + "?user_code=" + url.QueryEscape(da.UserCode)
	}
	show(p)
	tok, err := cfg.DeviceAccessToken(ctx, da, pollOpts...)
	if err != nil {
		e := oauthErr(err)
		var oe *OAuthError
		if errors.As(e, &oe) {
			switch oe.Code {
			case "access_denied":
				return nil, fmt.Errorf("device login: the person chose 拒绝 at %s: %w", p.VerificationURI, e)
			case "expired_token":
				return nil, fmt.Errorf("device login: the code %s expired before it was confirmed; start the login again: %w", da.UserCode, e)
			}
		}
		return nil, fmt.Errorf("device login: waiting for the confirmation at %s (POST %s): %w", p.VerificationURI, ep.TokenURL, e)
	}
	g := &Grant{AccessToken: tok.AccessToken, RefreshToken: tok.RefreshToken, expiry: tok.Expiry}
	if g.RefreshToken == "" || g.AccessToken == "" {
		return nil, fmt.Errorf("device login: POST %s answered without access_token or refresh_token (ZITADEL: the application needs the refresh token grant and the scope offline_access)", ep.TokenURL)
	}
	if err := s.Store.SaveRefresh(g.RefreshToken); err != nil {
		return nil, fmt.Errorf("device login: store the refresh token: %w", err)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if ep.zitadel() {
		if g.expiry.IsZero() {
			g.expiry = time.Now().Add(5 * time.Minute)
		}
		s.subject = g
		c, err := s.exchanged(ctx, AudienceCompute)
		if err != nil {
			return nil, fmt.Errorf("device login: signed in at ZITADEL, but the token service refused the device token: %w", err)
		}
		c.RefreshToken = g.RefreshToken
		s.remember(c)
		return c, nil
	}
	g.DeviceID, _ = tok.Extra("device_id").(string)
	g.Audience, _ = tok.Extra("audience").(string)
	if g.Audience == "" {
		g.Audience = AudienceCompute
	}
	s.remember(g)
	return g, nil
}

func oauthErr(err error) error {
	var re *oauth2.RetrieveError
	if errors.As(err, &re) {
		oe := &OAuthError{Code: re.ErrorCode, Description: re.ErrorDescription}
		if re.Response != nil {
			oe.Status = re.Response.StatusCode
		}
		var extra struct {
			Retryable bool   `json:"retryable"`
			Next      string `json:"next"`
		}
		if json.Unmarshal(re.Body, &extra) == nil {
			oe.Retryable, oe.Next = extra.Retryable, extra.Next
		}
		if oe.Code == "" {
			oe.Code, oe.Description = "unexpected_answer", clip(string(re.Body), 300)
		}
		return oe
	}
	return err
}

func (s *Session) remember(g *Grant) {
	if s.cache == nil {
		s.cache = map[string]*Grant{}
	}
	if g.expiry.IsZero() {
		g.expiry = time.Now().Add(time.Duration(g.ExpiresIn) * time.Second)
	}
	s.cache[g.Audience] = g
}

// Token returns a short token for aud (agentcompute or netbird) with its scope, from the cache while it has more
// than a minute left, otherwise by one refresh (NewAPI login) or one exchange (ZITADEL login).
func (s *Session) Token(ctx context.Context, aud string) (string, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if g, ok := s.cache[aud]; ok && time.Until(g.expiry) > time.Minute {
		return g.AccessToken, nil
	}
	var g *Grant
	var err error
	if s.Endpoints.zitadel() {
		g, err = s.exchanged(ctx, aud)
	} else {
		g, err = s.refresh(ctx, url.Values{"audience": {aud}, "scope": {strings.Join(ScopeFor(aud), " ")}})
		if g != nil && g.Audience == "" {
			g.Audience = aud
		}
	}
	if err != nil {
		return "", err
	}
	s.remember(g)
	return g.AccessToken, nil
}

// exchanged is an identity/v1 token for aud from ZITADEL's access token (s.mu held). ZITADEL's token is refreshed
// when it has less than a minute left, and once more when the STS refuses it as invalid_grant (expired or revoked
// meanwhile); a refused refresh at ZITADEL is ErrDeviceRemoved.
func (s *Session) exchanged(ctx context.Context, aud string) (*Grant, error) {
	refreshed := false
	if s.subject == nil || time.Until(s.subject.expiry) < time.Minute {
		if err := s.refreshSubject(ctx); err != nil {
			return nil, err
		}
		refreshed = true
	}
	g, err := s.exchange(ctx, s.subject.AccessToken, aud)
	var oe *OAuthError
	if err != nil && !refreshed && errors.As(err, &oe) && oe.Code == "invalid_grant" {
		if rerr := s.refreshSubject(ctx); rerr != nil {
			return nil, errors.Join(rerr, err)
		}
		g, err = s.exchange(ctx, s.subject.AccessToken, aud)
	}
	return g, err
}

// refreshSubject gets a new access token from ZITADEL with the refresh token (s.mu held).
func (s *Session) refreshSubject(ctx context.Context) error {
	g, err := s.refresh(ctx, nil)
	if err != nil {
		return err
	}
	if g.expiry.IsZero() {
		g.expiry = time.Now().Add(time.Duration(g.ExpiresIn) * time.Second)
	}
	s.subject = g
	return nil
}

// exchange is POST <sts>/v1/identity/exchange (RFC 8693) of ZITADEL's access token for aud with the device's scope.
// Exchanging is idempotent, so transport failures and 5xx are retried within 25 s.
func (s *Session) exchange(ctx context.Context, subject, aud string) (*Grant, error) {
	form := url.Values{
		"grant_type":         {"urn:ietf:params:oauth:grant-type:token-exchange"},
		"subject_token":      {subject},
		"subject_token_type": {"urn:ietf:params:oauth:token-type:jwt"},
		"audience":           {aud},
		"scope":              {strings.Join(ScopeFor(aud), " ")},
	}
	if n := exchangeLifetime[aud]; n > 0 {
		form.Set("expires_in", strconv.Itoa(n))
	}
	what := "identity/v1 exchange (aud " + aud + ")"
	start := time.Now()
	for wait := 2 * time.Second; ; wait *= 2 {
		g, retry, err := s.postForm(ctx, s.Endpoints.ExchangeURL, form, what)
		if err == nil {
			g.Audience = aud
			return g, nil
		}
		if !retry || time.Since(start)+wait > 25*time.Second {
			return nil, err
		}
		select {
		case <-ctx.Done():
			return nil, errors.Join(ctx.Err(), err)
		case <-time.After(wait):
		}
	}
}

// Revoke revokes the refresh token at ZITADEL (RFC 7009), when the computer is unbound: the login ends there too.
// The NewAPI login has nothing to revoke here (unbinding at compute/v1 removes it).
func (s *Session) Revoke(ctx context.Context) error {
	if s.Endpoints.RevokeURL == "" {
		return nil
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	rt, err := s.Store.LoadRefresh()
	if err != nil || rt == "" {
		return err
	}
	form := url.Values{"token": {rt}, "token_type_hint": {"refresh_token"}, "client_id": {s.Endpoints.ClientID}}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, s.Endpoints.RevokeURL, strings.NewReader(form.Encode()))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	resp, err := s.client().Do(req)
	if err != nil {
		return fmt.Errorf("revoke the device login: POST %s: %w (network)", s.Endpoints.RevokeURL, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		raw, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
		return fmt.Errorf("revoke the device login: POST %s answered %d: %s", s.Endpoints.RevokeURL, resp.StatusCode, clip(string(raw), 200))
	}
	s.subject, s.cache = nil, nil
	return nil
}

// Fresh drops the cached token of aud, so the next Token refreshes (a netbird token right before a login).
func (s *Session) Fresh(aud string) {
	s.mu.Lock()
	delete(s.cache, aud)
	s.mu.Unlock()
}

// refresh presents the newest refresh token at the token endpoint, with extra form fields (NewAPI login: audience and
// scope), s.mu held: one refresh at a time. The successor is stored before the access token is used. A lost answer
// is retried with the same refresh token (NewAPI: within the issuer's 30 s grace, which returns the same successor).
func (s *Session) refresh(ctx context.Context, extra url.Values) (*Grant, error) {
	rt, err := s.Store.LoadRefresh()
	if err != nil {
		return nil, err
	}
	if rt == "" {
		return nil, ErrNotLoggedIn
	}
	form := url.Values{"grant_type": {"refresh_token"}, "refresh_token": {rt}, "client_id": {s.Endpoints.ClientID}}
	for k, v := range extra {
		form[k] = v
	}
	what := "identity/v1 refresh"
	if a := form.Get("audience"); a != "" {
		what += " (aud " + a + ")"
	}
	if s.Endpoints.zitadel() {
		what = "ZITADEL refresh"
	}
	start := time.Now()
	var last error
	for wait := 2 * time.Second; ; wait *= 2 {
		g, retry, err := s.postForm(ctx, s.Endpoints.TokenURL, form, what)
		if err == nil {
			if g.RefreshToken == "" {
				if !s.Endpoints.zitadel() {
					return nil, fmt.Errorf("%s: the answer has no refresh_token", what)
				}
				g.RefreshToken = rt // not rotated: the same refresh token stays valid (RFC 6749 §6)
			} else if err := s.Store.SaveRefresh(g.RefreshToken); err != nil {
				return nil, fmt.Errorf("%s: store the new refresh token (the old one stops working): %w", what, err)
			}
			return g, nil
		}
		var oe *OAuthError
		if errors.As(err, &oe) && (oe.Code == "invalid_grant" || oe.Code == "invalid_client") {
			return nil, errors.Join(ErrDeviceRemoved, err)
		}
		last = err
		if !retry || time.Since(start)+wait > 25*time.Second {
			return nil, last
		}
		select {
		case <-ctx.Done():
			return nil, errors.Join(ctx.Err(), last)
		case <-time.After(wait):
		}
	}
}

// postForm sends one token request; retry says whether the same request may be sent again (transport failure, 5xx,
// temporarily_unavailable). Refusals are *OAuthError (RFC 6749 §5.2 plus the STS's retryable and next).
func (s *Session) postForm(ctx context.Context, endpoint string, form url.Values, what string) (*Grant, bool, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, strings.NewReader(form.Encode()))
	if err != nil {
		return nil, false, err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	resp, err := s.client().Do(req)
	if err != nil {
		return nil, true, fmt.Errorf("%s: POST %s: %w (network)", what, endpoint, err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode == http.StatusOK {
		var g Grant
		if err := json.Unmarshal(raw, &g); err != nil || g.AccessToken == "" {
			return nil, true, fmt.Errorf("%s: POST %s: the answer has no access_token: %s", what, endpoint, clip(string(raw), 200))
		}
		return &g, false, nil
	}
	oe := &OAuthError{Status: resp.StatusCode}
	if json.Unmarshal(raw, oe) != nil || oe.Code == "" {
		oe.Code, oe.Description, oe.Retryable = "unexpected_answer", clip(string(raw), 300), resp.StatusCode >= 500
	}
	base := fmt.Errorf("%s: POST %s: %w", what, endpoint, oe)
	return nil, oe.Code == "temporarily_unavailable" || resp.StatusCode >= 500, base
}

// UnverifiedClaims reads a token's claims without checking its signature: only for a token this device just got
// from the issuer over TLS (to learn its own ten and own), never for a token someone presents.
func UnverifiedClaims(token string) (*Claims, error) {
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		return nil, errors.New("identity/v1: not a compact JWS")
	}
	raw, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return nil, err
	}
	var c Claims
	if err := json.Unmarshal(raw, &c); err != nil {
		return nil, err
	}
	return &c, nil
}
