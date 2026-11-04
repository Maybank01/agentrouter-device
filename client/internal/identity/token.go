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
	"strings"
	"sync"
	"time"

	"golang.org/x/oauth2"
)

// Device login (identity/v1 README "Device login", RFC 8628). The device logs the person in once with a device code
// and keeps only the newest refresh token; every short token comes from POST /v1/identity/device/token with
// grant_type=refresh_token and the audience and scope it needs. The access token is never a long-lived credential and
// the device never calls POST /v1/identity/token.

// ClientID is AgentRouter's registered device client (identity/v1: registered for the AgentRouter product tenant).
const ClientID = "agentdevice"

// Audiences and the scope the device asks for in each (identity/v1 vectors/device.json client "agentdevice").
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

// OAuthError is the identity/v1 error body (RFC 6749 §5.2 plus retryable and next, R22).
type OAuthError struct {
	Status      int    `json:"-"`
	Code        string `json:"error"`
	Description string `json:"error_description"`
	Retryable   bool   `json:"retryable"`
	Next        string `json:"next"`
}

func (e *OAuthError) Error() string {
	msg := fmt.Sprintf("identity/v1 answered %d %s: %s", e.Status, e.Code, e.Description)
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
}

// EndpointsFor are the contract's endpoints under issuer.
func EndpointsFor(issuer string) Endpoints {
	base := strings.TrimSuffix(issuer, "/")
	return Endpoints{DeviceAuthURL: base + "/v1/identity/device/code", TokenURL: base + "/v1/identity/device/token", ClientID: ClientID}
}

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
}

func (s *Session) client() *http.Client {
	if s.HTTP != nil {
		return s.HTTP
	}
	return &http.Client{Timeout: 30 * time.Second}
}

// Login starts the device login (device_name shown to the person), calls show, polls until the person decides, and
// stores the refresh token before it returns. RFC 8628 answers (authorization_pending, slow_down, access_denied,
// expired_token) are handled by golang.org/x/oauth2's DeviceAccessToken.
func (s *Session) Login(ctx context.Context, deviceName string, show func(Prompt)) (*Grant, error) {
	cfg := &oauth2.Config{
		ClientID: s.Endpoints.ClientID,
		Endpoint: oauth2.Endpoint{DeviceAuthURL: s.Endpoints.DeviceAuthURL, TokenURL: s.Endpoints.TokenURL, AuthStyle: oauth2.AuthStyleInParams},
	}
	ctx = context.WithValue(ctx, oauth2.HTTPClient, s.client())
	da, err := cfg.DeviceAuth(ctx, oauth2.SetAuthURLParam("device_name", deviceName))
	if err != nil {
		return nil, fmt.Errorf("device login: POST %s: %w", s.Endpoints.DeviceAuthURL, oauthErr(err))
	}
	show(Prompt{UserCode: da.UserCode, VerificationURI: da.VerificationURI, VerificationURIComplete: da.VerificationURIComplete, Expiry: da.Expiry})
	tok, err := cfg.DeviceAccessToken(ctx, da,
		oauth2.SetAuthURLParam("audience", AudienceCompute),
		oauth2.SetAuthURLParam("scope", strings.Join(ScopeFor(AudienceCompute), " ")))
	if err != nil {
		e := oauthErr(err)
		var oe *OAuthError
		if errors.As(e, &oe) {
			switch oe.Code {
			case "access_denied":
				return nil, fmt.Errorf("device login: the person chose 拒绝 at %s: %w", da.VerificationURI, e)
			case "expired_token":
				return nil, fmt.Errorf("device login: the code %s expired before it was confirmed; start the login again: %w", da.UserCode, e)
			}
		}
		return nil, fmt.Errorf("device login: waiting for the confirmation at %s (POST %s): %w", da.VerificationURI, s.Endpoints.TokenURL, e)
	}
	g := &Grant{AccessToken: tok.AccessToken, RefreshToken: tok.RefreshToken, expiry: tok.Expiry}
	g.DeviceID, _ = tok.Extra("device_id").(string)
	g.Audience, _ = tok.Extra("audience").(string)
	if g.Audience == "" {
		g.Audience = AudienceCompute
	}
	if g.RefreshToken == "" || g.AccessToken == "" {
		return nil, fmt.Errorf("device login: POST %s answered without access_token or refresh_token", s.Endpoints.TokenURL)
	}
	if err := s.Store.SaveRefresh(g.RefreshToken); err != nil {
		return nil, fmt.Errorf("device login: store the refresh token: %w", err)
	}
	s.mu.Lock()
	s.remember(g)
	s.mu.Unlock()
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
// than a minute left, otherwise by one refresh.
func (s *Session) Token(ctx context.Context, aud string) (string, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if g, ok := s.cache[aud]; ok && time.Until(g.expiry) > time.Minute {
		return g.AccessToken, nil
	}
	g, err := s.refresh(ctx, aud, ScopeFor(aud))
	if err != nil {
		return "", err
	}
	s.remember(g)
	return g.AccessToken, nil
}

// Fresh drops the cached token of aud, so the next Token refreshes (a netbird token right before a login).
func (s *Session) Fresh(aud string) {
	s.mu.Lock()
	delete(s.cache, aud)
	s.mu.Unlock()
}

// refresh presents the newest refresh token (s.mu held: one refresh at a time). The successor is stored before the
// access token is used. A lost answer is retried with the same refresh token within the issuer's 30 s grace, which
// returns the same successor.
func (s *Session) refresh(ctx context.Context, aud string, scope []string) (*Grant, error) {
	rt, err := s.Store.LoadRefresh()
	if err != nil {
		return nil, err
	}
	if rt == "" {
		return nil, ErrNotLoggedIn
	}
	form := url.Values{
		"grant_type": {"refresh_token"}, "refresh_token": {rt}, "client_id": {s.Endpoints.ClientID},
		"audience": {aud}, "scope": {strings.Join(scope, " ")},
	}
	start := time.Now()
	var last error
	for attempt, wait := 0, 2*time.Second; ; attempt, wait = attempt+1, wait*2 {
		g, retry, err := s.post(ctx, form)
		if err == nil {
			if g.RefreshToken == "" {
				return nil, fmt.Errorf("identity/v1 refresh (aud %s): the answer has no refresh_token", aud)
			}
			if err := s.Store.SaveRefresh(g.RefreshToken); err != nil {
				return nil, fmt.Errorf("identity/v1 refresh: store the new refresh token (the old one stops working in 30 s): %w", err)
			}
			if g.Audience == "" {
				g.Audience = aud
			}
			return g, nil
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

// post sends one refresh; retry says whether the same refresh token may be presented again (transport failure,
// 5xx, temporarily_unavailable), and a refusal of the device itself is ErrDeviceRemoved.
func (s *Session) post(ctx context.Context, form url.Values) (*Grant, bool, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, s.Endpoints.TokenURL, strings.NewReader(form.Encode()))
	if err != nil {
		return nil, false, err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	resp, err := s.client().Do(req)
	if err != nil {
		return nil, true, fmt.Errorf("identity/v1 refresh: POST %s: %w (network)", s.Endpoints.TokenURL, err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode == http.StatusOK {
		var g Grant
		if err := json.Unmarshal(raw, &g); err != nil || g.AccessToken == "" {
			return nil, true, fmt.Errorf("identity/v1 refresh: POST %s: the answer has no access_token: %s", s.Endpoints.TokenURL, clip(string(raw), 200))
		}
		return &g, false, nil
	}
	oe := &OAuthError{Status: resp.StatusCode}
	if json.Unmarshal(raw, oe) != nil || oe.Code == "" {
		oe.Code, oe.Description, oe.Retryable = "unexpected_answer", clip(string(raw), 300), resp.StatusCode >= 500
	}
	base := fmt.Errorf("identity/v1 refresh (aud %s): POST %s: %w", form.Get("audience"), s.Endpoints.TokenURL, oe)
	switch {
	case oe.Code == "invalid_grant" || oe.Code == "invalid_client":
		return nil, false, errors.Join(ErrDeviceRemoved, base)
	case oe.Code == "temporarily_unavailable" || resp.StatusCode >= 500:
		return nil, true, base
	}
	return nil, false, base
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
