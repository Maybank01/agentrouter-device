package identity

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

type memStore struct {
	mu    sync.Mutex
	token string
	saves []string
}

func (m *memStore) LoadRefresh() (string, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.token, nil
}
func (m *memStore) SaveRefresh(t string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.token = t
	m.saves = append(m.saves, t)
	return nil
}

// issuer fakes identity/v1's device endpoints: rotation on every refresh, the 30 s grace for the previous token,
// invalid_grant for anything older (the device is logged out), and removal.
type issuer struct {
	mu       sync.Mutex
	polls    int
	current  string
	previous string
	n        int
	removed  bool
	refreshN atomic.Int32
	dropNext bool // drop the next answer after rotating (a lost answer)
	lastForm map[string]string
}

func (is *issuer) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	is.mu.Lock()
	defer is.mu.Unlock()
	_ = r.ParseForm()
	w.Header().Set("Content-Type", "application/json")
	fail := func(code string) {
		w.WriteHeader(http.StatusBadRequest)
		fmt.Fprintf(w, `{"error":%q,"error_description":"identity device token: %s","retryable":false,"next":"log in again"}`, code, code)
	}
	switch r.URL.Path {
	case "/v1/identity/device/code":
		if r.Form.Get("client_id") != ClientID || r.Form.Get("device_name") != "台式机" {
			fail("invalid_request")
			return
		}
		fmt.Fprint(w, `{"device_code":"dc","user_code":"BCDF-GHJK","verification_uri":"https://newapi.test/device","verification_uri_complete":"https://newapi.test/device?user_code=BCDF-GHJK","expires_in":600,"interval":1}`)
	case "/v1/identity/device/token":
		is.lastForm = map[string]string{}
		for k := range r.Form {
			is.lastForm[k] = r.Form.Get(k)
		}
		switch r.Form.Get("grant_type") {
		case "urn:ietf:params:oauth:grant-type:device_code":
			if is.polls++; is.polls == 1 {
				fail("authorization_pending")
				return
			}
			is.n++
			is.current = fmt.Sprintf("dev_a.s%d", is.n)
			fmt.Fprintf(w, `{"access_token":"at-%s","issued_token_type":"urn:ietf:params:oauth:token-type:jwt","token_type":"Bearer","expires_in":3600,"scope":"%s","audience":"agentcompute","refresh_token":%q,"device_id":"dev_a"}`, r.Form.Get("audience"), r.Form.Get("scope"), is.current)
		case "refresh_token":
			is.refreshN.Add(1)
			rt := r.Form.Get("refresh_token")
			switch {
			case is.removed:
				fail("invalid_grant")
				return
			case rt == is.current:
				is.previous = is.current
				is.n++
				is.current = fmt.Sprintf("dev_a.s%d", is.n)
			case rt == is.previous:
				// within the grace: the same successor
			default:
				fail("invalid_grant")
				return
			}
			if is.dropNext {
				is.dropNext = false
				w.WriteHeader(http.StatusServiceUnavailable)
				fmt.Fprint(w, `{"error":"temporarily_unavailable","error_description":"lost","retryable":true}`)
				return
			}
			fmt.Fprintf(w, `{"access_token":"at-%s-%d","issued_token_type":"urn:ietf:params:oauth:token-type:jwt","token_type":"Bearer","expires_in":3600,"scope":"%s","audience":%q,"refresh_token":%q,"device_id":"dev_a"}`, r.Form.Get("audience"), is.n, r.Form.Get("scope"), r.Form.Get("audience"), is.current)
		default:
			fail("unsupported_grant_type")
		}
	default:
		http.NotFound(w, r)
	}
}

func newSession(t *testing.T) (*Session, *issuer, *memStore) {
	t.Helper()
	is := &issuer{}
	srv := httptest.NewServer(is)
	t.Cleanup(srv.Close)
	st := &memStore{}
	return &Session{Endpoints: EndpointsFor(srv.URL), Store: st}, is, st
}

func TestLoginStoresTheRefreshTokenAndSendsTheDeviceName(t *testing.T) {
	s, is, st := newSession(t)
	var shown Prompt
	g, err := s.Login(context.Background(), "台式机", func(p Prompt) { shown = p })
	if err != nil {
		t.Fatal(err)
	}
	if shown.UserCode != "BCDF-GHJK" || shown.VerificationURIComplete == "" {
		t.Fatalf("prompt %+v", shown)
	}
	if st.token != "dev_a.s1" || g.DeviceID != "dev_a" || g.RefreshToken != "dev_a.s1" {
		t.Fatalf("stored %q, grant %+v", st.token, g)
	}
	if is.lastForm["audience"] != "agentcompute" || is.lastForm["scope"] != "device:*:enrol computer:*:report computer:*:manage" {
		t.Fatalf("login token request %v", is.lastForm)
	}
	// The login's access token is the agentcompute token; no refresh is needed for it.
	tok, err := s.Token(context.Background(), AudienceCompute)
	if err != nil || tok != "at-agentcompute" || is.refreshN.Load() != 0 {
		t.Fatalf("Token = %q, %v (refreshes %d)", tok, err, is.refreshN.Load())
	}
}

func TestRefreshRotatesAndStoresTheNewestToken(t *testing.T) {
	s, is, st := newSession(t)
	st.token = "dev_a.s0"
	is.current, is.n = "dev_a.s0", 0
	tok, err := s.Token(context.Background(), AudienceNetBird)
	if err != nil || !strings.HasPrefix(tok, "at-netbird-") {
		t.Fatalf("Token = %q, %v", tok, err)
	}
	if is.lastForm["grant_type"] != "refresh_token" || is.lastForm["client_id"] != ClientID || is.lastForm["scope"] != "computer:*:ssh" {
		t.Fatalf("refresh form %v", is.lastForm)
	}
	if st.token != "dev_a.s1" {
		t.Fatalf("stored %q, want the successor", st.token)
	}
	// Cached: a second call does not refresh.
	if _, err := s.Token(context.Background(), AudienceNetBird); err != nil || is.refreshN.Load() != 1 {
		t.Fatalf("second Token refreshed again (%d), %v", is.refreshN.Load(), err)
	}
	// Concurrent callers of another audience refresh one at a time, each with the newest token: no invalid_grant.
	s.Fresh(AudienceNetBird)
	var wg sync.WaitGroup
	errs := make(chan error, 8)
	for range 8 {
		wg.Go(func() {
			s.Fresh(AudienceCompute)
			if _, err := s.Token(context.Background(), AudienceCompute); err != nil {
				errs <- err
			}
		})
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		t.Fatalf("concurrent refresh: %v", err)
	}
	if len(st.saves) < 2 {
		t.Fatalf("saves %v", st.saves)
	}
}

func TestALostAnswerIsRetriedWithTheSameToken(t *testing.T) {
	s, is, st := newSession(t)
	st.token, is.current = "dev_a.s0", "dev_a.s0"
	is.dropNext = true
	start := time.Now()
	tok, err := s.Token(context.Background(), AudienceCompute)
	if err != nil {
		t.Fatalf("retry within the grace failed: %v", err)
	}
	if st.token != "dev_a.s1" || !strings.HasSuffix(tok, "-1") || time.Since(start) > 10*time.Second {
		t.Fatalf("stored %q, token %q", st.token, tok)
	}
}

func TestARemovedDeviceIsReported(t *testing.T) {
	s, is, st := newSession(t)
	st.token, is.current = "dev_a.s0", "dev_a.s0"
	is.removed = true
	_, err := s.Token(context.Background(), AudienceCompute)
	if !errors.Is(err, ErrDeviceRemoved) || !strings.Contains(err.Error(), "invalid_grant") {
		t.Fatalf("want ErrDeviceRemoved with the cause, got %v", err)
	}
	st.token = ""
	if _, err := s.Token(context.Background(), AudienceCompute); !errors.Is(err, ErrNotLoggedIn) {
		t.Fatalf("want ErrNotLoggedIn, got %v", err)
	}
}
