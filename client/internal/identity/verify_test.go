package identity

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// The provider's vectors, vendored byte for byte (testdata/contracts/agentrouter-newapi, SHA256SUMS there).
var vectorDir = filepath.Join("..", "..", "testdata", "contracts", "agentrouter-newapi", "contracts", "identity", "v1", "vectors")

func loadKeys(t *testing.T) StaticKeys {
	t.Helper()
	raw, err := os.ReadFile(filepath.Join(vectorDir, "TEST-ONLY-jwks.json"))
	if err != nil {
		t.Fatal(err)
	}
	keys, err := ParseJWKS(raw)
	if err != nil {
		t.Fatal(err)
	}
	return keys
}

func TestContractVectors(t *testing.T) {
	raw, err := os.ReadFile(filepath.Join(vectorDir, "tokens.json"))
	if err != nil {
		t.Fatal(err)
	}
	var file struct {
		Iss   string `json:"iss"`
		Now   int64  `json:"now"`
		Cases []struct {
			Name   string `json:"name"`
			Token  string `json:"token"`
			Verify struct {
				Aud   string `json:"aud"`
				Scope string `json:"scope"`
				Now   int64  `json:"now"`
			} `json:"verify"`
			Valid bool   `json:"valid"`
			Error string `json:"error"`
		} `json:"cases"`
	}
	if err := json.Unmarshal(raw, &file); err != nil {
		t.Fatal(err)
	}
	if len(file.Cases) < 20 {
		t.Fatalf("only %d vectors", len(file.Cases))
	}
	keys := loadKeys(t)
	for _, c := range file.Cases {
		t.Run(c.Name, func(t *testing.T) {
			now := file.Now
			if c.Verify.Now != 0 {
				now = c.Verify.Now
			}
			v := &Verifier{Issuer: file.Iss, Keys: keys, Now: func() time.Time { return time.Unix(now, 0) }}
			_, err := v.Verify(context.Background(), c.Token, c.Verify.Aud, c.Verify.Scope)
			if c.Valid {
				if err != nil {
					t.Fatalf("want valid, got %v", err)
				}
				return
			}
			if err == nil {
				t.Fatalf("want rejected (%s), got valid", c.Error)
			}
			if got := Check(err); got != c.Error {
				t.Fatalf("want check %q, got %q (%v)", c.Error, got, err)
			}
		})
	}
}

func TestCovers(t *testing.T) {
	cases := []struct {
		held []string
		want string
		ok   bool
	}{
		{[]string{"computer:cmp_a:ssh"}, "computer:cmp_a:ssh", true},
		{[]string{"computer:*:ssh"}, "computer:cmp_a:ssh", true},
		{[]string{"computer:cmp_b:ssh"}, "computer:cmp_a:ssh", false},
		{[]string{"computer:cmp_a:read"}, "computer:cmp_a:ssh", false},
		{[]string{"space:*:rw"}, "space:spc_x:ro", true},
		{[]string{"space:*:rw"}, "space:new:create", false},
		{[]string{"space:spc_x:ro"}, "space:spc_x:rw", false},
		{[]string{"bad"}, "computer:cmp_a:ssh", false},
	}
	for _, c := range cases {
		if got := Covers(c.held, c.want); got != c.ok {
			t.Errorf("Covers(%v, %s) = %v, want %v", c.held, c.want, got, c.ok)
		}
	}
}

func TestRemoteKeysCachesAndRefetchesOnUnknownKid(t *testing.T) {
	raw, err := os.ReadFile(filepath.Join(vectorDir, "TEST-ONLY-jwks.json"))
	if err != nil {
		t.Fatal(err)
	}
	var hits atomic.Int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		w.Header().Set("Cache-Control", "public, max-age=600")
		_, _ = w.Write(raw)
	}))
	defer srv.Close()
	r := &RemoteKeys{URL: srv.URL + "/.well-known/jwks.json"}
	ctx := context.Background()
	if _, err := r.Key(ctx, "test-2026-11-a"); err != nil {
		t.Fatal(err)
	}
	if _, err := r.Key(ctx, "test-2026-11-b"); err != nil {
		t.Fatal(err)
	}
	if hits.Load() != 1 {
		t.Fatalf("fetched %d times, want 1 (cached)", hits.Load())
	}
	if _, err := r.Key(ctx, "nope"); err == nil {
		t.Fatal("unknown kid accepted")
	}
	if _, err := r.Key(ctx, "nope"); err == nil {
		t.Fatal("unknown kid accepted")
	}
	if hits.Load() != 1 {
		t.Fatalf("an unknown kid refetched within %s (%d fetches)", RefetchInterval, hits.Load())
	}
}

func TestIssueSendsCredentialAndReportsOAuthErrors(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/identity/token" || r.Header.Get("Authorization") != "Bearer cred-1" {
			w.WriteHeader(http.StatusUnauthorized)
			fmt.Fprint(w, `{"error":"invalid_client","error_description":"identity token: credential unknown","retryable":false,"next":"log in again"}`)
			return
		}
		var body struct {
			Aud string   `json:"aud"`
			Scp []string `json:"scp"`
		}
		_ = json.NewDecoder(r.Body).Decode(&body)
		fmt.Fprintf(w, `{"access_token":"tok-%s-%s","issued_token_type":"urn:ietf:params:oauth:token-type:jwt","token_type":"Bearer","expires_in":600,"scope":"%s"}`, body.Aud, body.Scp[0], body.Scp[0])
	}))
	defer srv.Close()
	c := &Client{Issuer: srv.URL, Credential: "cred-1"}
	tok, err := c.Issue(context.Background(), "agentcompute", []string{"device:*:enrol"}, 600)
	if err != nil || tok != "tok-agentcompute-device:*:enrol" {
		t.Fatalf("Issue = %q, %v", tok, err)
	}
	c.Credential = "wrong"
	_, err = c.Issue(context.Background(), "agentcompute", []string{"device:*:enrol"}, 600)
	if err == nil || !strings.Contains(err.Error(), "invalid_client") || !strings.Contains(err.Error(), "log in again") {
		t.Fatalf("want the full OAuth error, got %v", err)
	}
}

func TestDeviceLogin(t *testing.T) {
	var polls atomic.Int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		switch r.URL.Path {
		case "/v1/identity/device/code":
			fmt.Fprint(w, `{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri":"https://newapi.test/device","verification_uri_complete":"https://newapi.test/device?code=ABCD-EFGH","expires_in":600,"interval":1}`)
		case "/v1/identity/device/token":
			_ = r.ParseForm()
			if r.Form.Get("device_code") != "dc" {
				w.WriteHeader(http.StatusBadRequest)
				fmt.Fprint(w, `{"error":"invalid_grant"}`)
				return
			}
			if polls.Add(1) == 1 {
				w.WriteHeader(http.StatusBadRequest)
				fmt.Fprint(w, `{"error":"authorization_pending"}`)
				return
			}
			fmt.Fprint(w, `{"access_token":"device-credential","token_type":"Bearer"}`)
		default:
			http.NotFound(w, r)
		}
	}))
	defer srv.Close()
	var shown Prompt
	cred, err := DeviceLoginFor(srv.URL).Login(context.Background(), func(p Prompt) { shown = p })
	if err != nil || cred != "device-credential" {
		t.Fatalf("Login = %q, %v", cred, err)
	}
	if shown.UserCode != "ABCD-EFGH" || shown.VerificationURIComplete == "" {
		t.Fatalf("prompt %+v", shown)
	}
}
