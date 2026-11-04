package identity

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"

	"golang.org/x/oauth2"
)

// Client asks the issuer (NewAPI's identity plugin) for tokens with the device's own credential.
type Client struct {
	// Issuer is the NewAPI base URL (the tokens' iss).
	Issuer string
	// Credential is the account credential the device-code login gave this device; sent as a bearer token to
	// POST /v1/identity/token (identity/v1 security scheme apiKey).
	Credential string
	HTTP       *http.Client
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

// Issue asks POST /v1/identity/token for a token of the device's account: sub = ten = own = pay = that account.
func (c *Client) Issue(ctx context.Context, aud string, scp []string, expiresIn int) (string, error) {
	body, _ := json.Marshal(map[string]any{"aud": aud, "scp": scp, "expires_in": expiresIn})
	url := strings.TrimSuffix(c.Issuer, "/") + "/v1/identity/token"
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(body))
	if err != nil {
		return "", fmt.Errorf("identity token (aud %s, scp %v): %w", aud, scp, err)
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+c.Credential)
	client := c.HTTP
	if client == nil {
		client = &http.Client{Timeout: 30 * time.Second}
	}
	resp, err := client.Do(req)
	if err != nil {
		return "", fmt.Errorf("identity token: POST %s (aud %s): %w (network; retry)", url, aud, err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode != http.StatusOK {
		oe := &OAuthError{Status: resp.StatusCode}
		if json.Unmarshal(raw, oe) != nil || oe.Code == "" {
			oe.Code, oe.Description = "unexpected_answer", clip(string(raw), 300)
			oe.Retryable = resp.StatusCode >= 500
		}
		return "", fmt.Errorf("identity token: POST %s (aud %s, scp %v): %w", url, aud, scp, oe)
	}
	var out struct {
		AccessToken string `json:"access_token"`
	}
	if err := json.Unmarshal(raw, &out); err != nil || out.AccessToken == "" {
		return "", fmt.Errorf("identity token: POST %s: the answer has no access_token: %s", url, clip(string(raw), 300))
	}
	return out.AccessToken, nil
}

// DeviceLogin is the device-code login (RFC 8628) to the person's NewAPI account. Its endpoints are not in
// identity/v1 yet (proposed additive there: POST <issuer>/v1/identity/device/code and /v1/identity/device/token);
// the grant's access token is the device's long-lived account credential, used only to ask POST /v1/identity/token.
type DeviceLogin struct {
	ClientID      string
	DeviceAuthURL string
	TokenURL      string
	HTTP          *http.Client
}

// DeviceLoginFor returns the login with the proposed default endpoints under issuer.
func DeviceLoginFor(issuer string) DeviceLogin {
	base := strings.TrimSuffix(issuer, "/")
	return DeviceLogin{
		ClientID:      "agentdevice",
		DeviceAuthURL: base + "/v1/identity/device/code",
		TokenURL:      base + "/v1/identity/device/token",
	}
}

// Prompt is what the person must see to confirm the login in their browser.
type Prompt struct {
	UserCode                string
	VerificationURI         string
	VerificationURIComplete string
	Expiry                  time.Time
}

// Login starts the device authorization, calls show with what the person must do, and waits until they confirm (or
// the code expires). It returns the device credential.
func (d DeviceLogin) Login(ctx context.Context, show func(Prompt)) (string, error) {
	cfg := &oauth2.Config{
		ClientID: d.ClientID,
		Endpoint: oauth2.Endpoint{DeviceAuthURL: d.DeviceAuthURL, TokenURL: d.TokenURL, AuthStyle: oauth2.AuthStyleInParams},
	}
	if d.HTTP != nil {
		ctx = context.WithValue(ctx, oauth2.HTTPClient, d.HTTP)
	}
	da, err := cfg.DeviceAuth(ctx)
	if err != nil {
		return "", fmt.Errorf("device login: POST %s: %w", d.DeviceAuthURL, err)
	}
	show(Prompt{UserCode: da.UserCode, VerificationURI: da.VerificationURI, VerificationURIComplete: da.VerificationURIComplete, Expiry: da.Expiry})
	tok, err := cfg.DeviceAccessToken(ctx, da)
	if err != nil {
		return "", fmt.Errorf("device login: waiting for the confirmation at %s (POST %s): %w", da.VerificationURI, d.TokenURL, err)
	}
	if tok.AccessToken == "" {
		return "", fmt.Errorf("device login: POST %s answered without an access token", d.TokenURL)
	}
	return tok.AccessToken, nil
}
