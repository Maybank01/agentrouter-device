package update

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func TestVersion(t *testing.T) {
	for in, want := range map[string]bool{
		"0.1.0": true, "client-v1.2.3": true, "v10.0.1": true,
		"dev": false, "0.0.0-abc1234": false, "client-v0.0.0-abc1234": false, "1.2": false, "1.02.3": false, "1.-2.3": false,
	} {
		if _, ok := Version(in); ok != want {
			t.Errorf("Version(%q) ok = %v, want %v", in, ok, want)
		}
	}
	a, _ := Version("0.9.9")
	b, _ := Version("0.10.0")
	if !Newer(a, b) || Newer(b, a) || Newer(a, a) {
		t.Fatal("Newer compares by number, not text")
	}
}

func serve(t *testing.T, status int, body string) string {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !strings.HasPrefix(r.Header.Get("User-Agent"), "agentdevice/") {
			t.Errorf("User-Agent %q", r.Header.Get("User-Agent"))
		}
		w.WriteHeader(status)
		_, _ = w.Write([]byte(body))
	}))
	t.Cleanup(srv.Close)
	return srv.URL
}

func TestCheck(t *testing.T) {
	ctx := context.Background()
	url := serve(t, 200, `{"tag_name":"client-v0.2.0","html_url":"https://example.test/releases/tag/client-v0.2.0"}`)
	r, err := Check(ctx, http.DefaultClient, url, "0.1.0")
	if err != nil || r == nil || r.Version != "0.2.0" {
		t.Fatalf("newer: %+v, %v", r, err)
	}
	if got := Notice(r, "https://dl.test/agentdevice-setup-dev.exe"); !strings.Contains(got, "0.2.0") || !strings.Contains(got, "agentdevice-setup-dev.exe") {
		t.Fatalf("notice %q", got)
	}
	if got := Notice(r, ""); !strings.Contains(got, r.Page) {
		t.Fatalf("notice without a download link %q", got)
	}
	for _, cur := range []string{"0.2.0", "0.3.0"} {
		if r, err := Check(ctx, http.DefaultClient, url, cur); err != nil || r != nil {
			t.Fatalf("current %s: %+v, %v", cur, r, err)
		}
	}
	// Developer and pre-release builds never ask.
	for _, cur := range []string{"dev", "0.0.0-abc1234"} {
		if r, err := Check(ctx, http.DefaultClient, "http://127.0.0.1:1/unreachable", cur); err != nil || r != nil {
			t.Fatalf("build %s asked: %+v, %v", cur, r, err)
		}
	}
	if _, err := Check(ctx, http.DefaultClient, serve(t, 404, `{"message":"Not Found"}`), "0.1.0"); err == nil || !strings.Contains(err.Error(), "404") {
		t.Fatalf("404: %v", err)
	}
	if _, err := Check(ctx, http.DefaultClient, serve(t, 200, `{"tag_name":"client-v0.0.0-abc1234"}`), "0.1.0"); err == nil {
		t.Fatal("a pre-release tag must not count as a release")
	}
}

func TestWatchTellsOncePerVersion(t *testing.T) {
	url := serve(t, 200, `{"tag_name":"client-v0.2.0","html_url":"https://example.test/r"}`)
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	told := 0
	Watch(ctx, url, "0.1.0", time.Millisecond, 20*time.Millisecond, func(*Release) { told++ }, func(err error) { t.Error(err) })
	if told != 1 {
		t.Fatalf("told %d times, want once", told)
	}
}
