package identity

import (
	"context"
	"encoding/base64"
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

// zitadelFake fakes ZITADEL's device flow (device_authorization, token with the device code and the refresh token,
// revoke) and the STS's exchange: the access tokens are JWT-shaped with the claims the Action appends, ZITADEL rotates
// the refresh token on every use, and the exchange refuses an access token ZITADEL has replaced (invalid_grant).
type zitadelFake struct {
	mu        sync.Mutex
	polls     int
	n         int
	refresh   string
	access    string
	expiresIn int
	revoked   bool
	refreshN  int
	exchanges []map[string]string
	starts    []map[string]string
	revokes   []map[string]string
}

const testProject, testClient = "398174617035870308", "398174617103044708"

func fakeJWT(claims string) string {
	enc := base64.RawURLEncoding.EncodeToString
	return enc([]byte(`{"alg":"ES256","typ":"at+jwt","kid":"k"}`)) + "." + enc([]byte(claims)) + ".c2ln"
}

func formOf(r *http.Request) map[string]string {
	m := map[string]string{}
	for k := range r.Form {
		m[k] = r.Form.Get(k)
	}
	return m
}

func (z *zitadelFake) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	z.mu.Lock()
	defer z.mu.Unlock()
	_ = r.ParseForm()
	w.Header().Set("Content-Type", "application/json")
	fail := func(code string) {
		w.WriteHeader(http.StatusBadRequest)
		fmt.Fprintf(w, `{"error":%q,"error_description":"fake: %s"}`, code, code)
	}
	issue := func() {
		z.n++
		z.refresh = fmt.Sprintf("zrt-%d", z.n)
		z.access = fakeJWT(fmt.Sprintf(`{"iss":"zitadel","sub":"398","n":%d,"own":"acc_398","pay":"acc_398","ten":"acc_398174613395214436"}`, z.n))
		fmt.Fprintf(w, `{"access_token":%q,"token_type":"Bearer","expires_in":%d,"refresh_token":%q,"id_token":"x"}`, z.access, z.expiresIn, z.refresh)
	}
	switch r.URL.Path {
	case "/oauth/v2/device_authorization":
		z.starts = append(z.starts, formOf(r))
		if r.Form.Get("client_id") != testClient {
			fail("invalid_client")
			return
		}
		fmt.Fprint(w, `{"device_code":"dc","user_code":"BCDF-GHJK","verification_uri":"http://z/device","verification_uri_complete":"http://z/device?user_code=BCDF-GHJK","expires_in":600,"interval":1}`)
	case "/oauth/v2/token":
		if r.Form.Get("client_id") != testClient {
			fail("invalid_client")
			return
		}
		switch r.Form.Get("grant_type") {
		case "urn:ietf:params:oauth:grant-type:device_code":
			if z.polls++; z.polls == 1 {
				fail("authorization_pending")
				return
			}
			issue()
		case "refresh_token":
			z.refreshN++
			if z.revoked || r.Form.Get("refresh_token") != z.refresh {
				fail("invalid_grant")
				return
			}
			issue()
		default:
			fail("unsupported_grant_type")
		}
	case "/oauth/v2/revoke":
		z.revokes = append(z.revokes, formOf(r))
		z.revoked = true
		w.WriteHeader(http.StatusOK)
	case "/v1/identity/exchange":
		f := formOf(r)
		z.exchanges = append(z.exchanges, f)
		if f["grant_type"] != "urn:ietf:params:oauth:grant-type:token-exchange" || f["subject_token_type"] != "urn:ietf:params:oauth:token-type:jwt" {
			fail("invalid_request")
			return
		}
		if f["subject_token"] != z.access {
			w.WriteHeader(http.StatusBadRequest)
			fmt.Fprint(w, `{"error":"invalid_grant","error_description":"identity/v1 exchange: subject token: expired","retryable":false,"next":"sign in at ZITADEL again or refresh the access token"}`)
			return
		}
		c, _ := UnverifiedClaims(z.access)
		tok := fakeJWT(fmt.Sprintf(`{"iss":"https://sts.test","sub":%q,"own":%q,"pay":%q,"ten":%q,"aud":%q}`, c.Own, c.Own, c.Pay, c.Ten, f["audience"]))
		fmt.Fprintf(w, `{"access_token":%q,"issued_token_type":"urn:ietf:params:oauth:token-type:jwt","token_type":"Bearer","expires_in":%s,"scope":%q}`, tok, f["expires_in"], f["scope"])
	default:
		http.NotFound(w, r)
	}
}

func newZitadelSession(t *testing.T) (*Session, *zitadelFake, *memStore) {
	t.Helper()
	z := &zitadelFake{expiresIn: 43200}
	srv := httptest.NewServer(z)
	t.Cleanup(srv.Close)
	st := &memStore{}
	return &Session{Endpoints: ZitadelEndpoints(srv.URL+"/", testProject, testClient, srv.URL+"/v1/identity/exchange"), Store: st}, z, st
}

func TestZitadelLoginExchangesForAnAgentComputeToken(t *testing.T) {
	s, z, st := newZitadelSession(t)
	var shown Prompt
	g, err := s.Login(context.Background(), "台式机", func(p Prompt) { shown = p })
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasSuffix(shown.VerificationURI, "/ui/v2/login/device") || shown.VerificationURIComplete != shown.VerificationURI+"?user_code=BCDF-GHJK" || shown.UserCode != "BCDF-GHJK" {
		t.Fatalf("prompt %+v: want Login v2's device page with the code", shown)
	}
	start := z.starts[0]
	if start["scope"] != "openid offline_access urn:zitadel:iam:org:project:id:"+testProject+":aud" || start["device_name"] != "" {
		t.Fatalf("device authorization form %v", start)
	}
	if st.token != "zrt-1" || g.RefreshToken != "zrt-1" {
		t.Fatalf("stored %q, grant refresh %q", st.token, g.RefreshToken)
	}
	if len(z.exchanges) != 1 {
		t.Fatalf("exchanges %d, want 1", len(z.exchanges))
	}
	ex := z.exchanges[0]
	if ex["audience"] != AudienceCompute || ex["scope"] != "device:*:enrol computer:*:report computer:*:manage" || ex["expires_in"] != "3600" {
		t.Fatalf("exchange form %v", ex)
	}
	c, err := UnverifiedClaims(g.AccessToken)
	if err != nil || c.Iss != "https://sts.test" || c.Ten != "acc_398174613395214436" || c.Own != "acc_398" || g.DeviceID != "" {
		t.Fatalf("login grant claims %+v, %v", c, err)
	}
	// The login's token is the cached agentcompute token: no refresh, no second exchange.
	tok, err := s.Token(context.Background(), AudienceCompute)
	if err != nil || tok != g.AccessToken || z.refreshN != 0 || len(z.exchanges) != 1 {
		t.Fatalf("Token = %v (refreshes %d, exchanges %d)", err, z.refreshN, len(z.exchanges))
	}
	// A netbird token is a second exchange of the same ZITADEL token, scope computer:*:ssh, 600 s.
	if _, err := s.Token(context.Background(), AudienceNetBird); err != nil {
		t.Fatal(err)
	}
	if ex := z.exchanges[1]; ex["audience"] != AudienceNetBird || ex["scope"] != "computer:*:ssh" || ex["expires_in"] != "600" || z.refreshN != 0 {
		t.Fatalf("netbird exchange %v (refreshes %d)", ex, z.refreshN)
	}
}

func TestZitadelRefreshesWhenTheAccessTokenRunsOut(t *testing.T) {
	s, z, st := newZitadelSession(t)
	// A device that logged in earlier: only the refresh token survives a restart.
	z.refresh, st.token = "zrt-0", "zrt-0"
	if _, err := s.Token(context.Background(), AudienceCompute); err != nil {
		t.Fatal(err)
	}
	if z.refreshN != 1 || st.token != "zrt-1" {
		t.Fatalf("refreshes %d, stored %q: want one refresh and the rotated token stored", z.refreshN, st.token)
	}
	// ZITADEL replaced the access token meanwhile (expired, revoked session): the STS says invalid_grant, the device
	// refreshes once and exchanges again.
	z.access = "replaced"
	s.Fresh(AudienceNetBird)
	if _, err := s.Token(context.Background(), AudienceNetBird); err != nil {
		t.Fatalf("exchange after invalid_grant: %v", err)
	}
	if z.refreshN != 2 || st.token != "zrt-2" {
		t.Fatalf("refreshes %d, stored %q", z.refreshN, st.token)
	}
	// An access token near its end is refreshed before the exchange.
	z.expiresIn = 30
	s.subject.expiry = time.Now().Add(30 * time.Second)
	s.Fresh(AudienceCompute)
	if _, err := s.Token(context.Background(), AudienceCompute); err != nil || z.refreshN != 3 {
		t.Fatalf("refreshes %d, %v", z.refreshN, err)
	}
}

func TestZitadelARevokedLoginIsARemovedDevice(t *testing.T) {
	s, z, st := newZitadelSession(t)
	z.refresh, st.token = "zrt-0", "zrt-0"
	if err := s.Revoke(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(z.revokes) != 1 || z.revokes[0]["token"] != "zrt-0" || z.revokes[0]["client_id"] != testClient || z.revokes[0]["token_type_hint"] != "refresh_token" {
		t.Fatalf("revoke form %v", z.revokes)
	}
	_, err := s.Token(context.Background(), AudienceCompute)
	if !errors.Is(err, ErrDeviceRemoved) || !strings.Contains(err.Error(), "ZITADEL refresh") || !strings.Contains(err.Error(), "invalid_grant") {
		t.Fatalf("want ErrDeviceRemoved naming ZITADEL's refusal, got %v", err)
	}
}

func TestZitadelAnExchangeRefusalIsNotARemovedDevice(t *testing.T) {
	s, z, st := newZitadelSession(t)
	z.refresh, st.token = "zrt-0", "zrt-0"
	s.Endpoints.ExchangeURL = strings.Replace(s.Endpoints.ExchangeURL, "/v1/identity/exchange", "/v1/identity/nowhere", 1)
	_, err := s.Token(context.Background(), AudienceCompute)
	if err == nil || errors.Is(err, ErrDeviceRemoved) || !strings.Contains(err.Error(), "identity/v1 exchange (aud agentcompute)") {
		t.Fatalf("want the exchange's own error, got %v", err)
	}
}
