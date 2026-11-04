package egress

import (
	"bufio"
	"crypto/tls"
	"crypto/x509"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync"
	"testing"
)

// fakeProxy answers CONNECT example.com:443 by dialing the test TLS server; anything else 403.
func fakeProxy(t *testing.T, upstream string) (addr string, seen func() []string) {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { ln.Close() })
	var mu sync.Mutex
	var targets []string
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			go func(c net.Conn) {
				defer c.Close()
				br := bufio.NewReader(c)
				req, err := http.ReadRequest(br)
				if err != nil {
					return
				}
				mu.Lock()
				targets = append(targets, req.Method+" "+req.Host+" "+req.Header.Get("Proxy-Authorization"))
				mu.Unlock()
				if req.Method != http.MethodConnect || req.Host != "example.com:443" {
					io.WriteString(c, "HTTP/1.1 403 Forbidden\r\n\r\n")
					return
				}
				up, err := net.Dial("tcp", upstream)
				if err != nil {
					io.WriteString(c, "HTTP/1.1 502 Bad Gateway\r\n\r\n")
					return
				}
				defer up.Close()
				io.WriteString(c, "HTTP/1.1 200 Connection established\r\n\r\n")
				go io.Copy(up, br)
				io.Copy(c, up)
			}(c)
		}
	}()
	return ln.Addr().String(), func() []string { mu.Lock(); defer mu.Unlock(); return append([]string(nil), targets...) }
}

func startRelay(t *testing.T, proxyAddr string, hosts ...string) string {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { ln.Close() })
	u, _ := url.Parse("http://user:secret@" + proxyAddr)
	go (&Relay{Proxy: u, Hosts: hosts}).Serve(ln)
	return ln.Addr().String()
}

func TestRelayCarriesTLSThroughTheProxy(t *testing.T) {
	srv := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		io.WriteString(w, "hello through "+r.Host)
	}))
	defer srv.Close()
	proxyAddr, seen := fakeProxy(t, srv.Listener.Addr().String())
	relay := startRelay(t, proxyAddr, "example.com")

	pool := x509.NewCertPool()
	pool.AddCert(srv.Certificate())
	conn, err := tls.Dial("tcp", relay, &tls.Config{RootCAs: pool, ServerName: "example.com"})
	if err != nil {
		t.Fatalf("TLS through the relay: %v", err)
	}
	defer conn.Close()
	if err := conn.VerifyHostname("example.com"); err != nil {
		t.Fatalf("certificate: %v", err)
	}
	io.WriteString(conn, "GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
	resp, err := http.ReadResponse(bufio.NewReader(conn), nil)
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(resp.Body)
	if !strings.Contains(string(body), "hello through example.com") {
		t.Fatalf("body %q", body)
	}
	got := seen()
	if len(got) != 1 || !strings.HasPrefix(got[0], "CONNECT example.com:443 Basic ") {
		t.Fatalf("proxy saw %v", got)
	}
}

func TestRelayRefusesOtherNames(t *testing.T) {
	srv := httptest.NewTLSServer(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {}))
	defer srv.Close()
	proxyAddr, seen := fakeProxy(t, srv.Listener.Addr().String())
	relay := startRelay(t, proxyAddr, "netbird.example")
	pool := x509.NewCertPool()
	pool.AddCert(srv.Certificate())
	if c, err := tls.Dial("tcp", relay, &tls.Config{RootCAs: pool, ServerName: "example.com"}); err == nil {
		c.Close()
		t.Fatal("a name that is not listed was relayed")
	}
	if got := seen(); len(got) != 0 {
		t.Fatalf("the proxy was asked: %v", got)
	}
}

func TestServerNameNeedsAName(t *testing.T) {
	a, b := net.Pipe()
	defer a.Close()
	go func() {
		c := tls.Client(b, &tls.Config{InsecureSkipVerify: true}) // no ServerName: the hello has no SNI
		_ = c.Handshake()
		b.Close()
	}()
	if _, _, err := ServerName(a); err == nil {
		t.Fatal("a hello without a server name was accepted")
	}
}
