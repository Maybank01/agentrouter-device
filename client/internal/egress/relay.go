// Package egress relays the network client's own TLS connections through an HTTP CONNECT proxy, for a computer whose
// only way out is such a proxy (a compute/v1 sandbox on a Daytona node: compute/v1 README §7.1, sandbox-join
// `proxy`). NetBird's relay dialer (WebSocket) does not use proxy variables, so the provider maps NetBird's host names
// to 127.0.0.1 and the Relay listens there on 443: it reads the TLS ClientHello's server name, refuses names it was not
// given, opens `CONNECT <name>:443` at the proxy and then copies bytes both ways. TLS stays end to end (the relay never
// decrypts); the certificate is checked by NetBird against the real name.
package egress

import (
	"bufio"
	"bytes"
	"context"
	"crypto/tls"
	"encoding/base64"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"time"
)

// Relay forwards TLS connections by SNI through an HTTP CONNECT proxy.
type Relay struct {
	// Proxy is http://[user:password@]host:port.
	Proxy *url.URL
	// Hosts are the server names relayed (lower case); any other name is refused.
	Hosts []string
	// Port is the port asked for at the proxy (default 443).
	Port int
	Log  *slog.Logger
	// Dial reaches the proxy (default net.Dialer).
	Dial func(ctx context.Context, network, addr string) (net.Conn, error)
}

// errHelloRead stops the TLS handshake once the ClientHello is read.
var errHelloRead = errors.New("egress: client hello read")

// Serve accepts until ln is closed.
func (r *Relay) Serve(ln net.Listener) error {
	for {
		c, err := ln.Accept()
		if err != nil {
			if errors.Is(err, net.ErrClosed) {
				return nil
			}
			var ne net.Error
			if errors.As(err, &ne) && ne.Timeout() {
				time.Sleep(50 * time.Millisecond)
				continue
			}
			return err
		}
		go r.handle(c)
	}
}

func (r *Relay) log() *slog.Logger {
	if r.Log != nil {
		return r.Log
	}
	return slog.Default()
}

func (r *Relay) allowed(name string) bool {
	name = strings.ToLower(strings.TrimSuffix(name, "."))
	for _, h := range r.Hosts {
		if strings.EqualFold(h, name) {
			return true
		}
	}
	return false
}

func (r *Relay) handle(c net.Conn) {
	defer c.Close()
	_ = c.SetDeadline(time.Now().Add(15 * time.Second))
	name, hello, err := ServerName(c)
	if err != nil {
		r.log().Warn("egress: no TLS client hello", "from", c.RemoteAddr().String(), "error", err.Error())
		return
	}
	if !r.allowed(name) {
		r.log().Warn("egress: server name not relayed", "name", name)
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	up, rest, err := r.connect(ctx, name)
	cancel()
	if err != nil {
		r.log().Warn("egress: proxy CONNECT failed", "name", name, "proxy", r.Proxy.Host, "error", err.Error())
		return
	}
	defer up.Close()
	_ = c.SetDeadline(time.Time{})
	if _, err := up.Write(hello); err != nil {
		return
	}
	if len(rest) > 0 {
		if _, err := c.Write(rest); err != nil {
			return
		}
	}
	splice(c, up)
}

// connect opens a tunnel to name:port at the proxy. rest is what the proxy already sent past its answer.
func (r *Relay) connect(ctx context.Context, name string) (net.Conn, []byte, error) {
	dial := r.Dial
	if dial == nil {
		dial = (&net.Dialer{Timeout: 10 * time.Second}).DialContext
	}
	port := r.Port
	if port == 0 {
		port = 443
	}
	addr := r.Proxy.Host
	if r.Proxy.Port() == "" {
		addr = net.JoinHostPort(r.Proxy.Hostname(), "80")
	}
	up, err := dial(ctx, "tcp", addr)
	if err != nil {
		return nil, nil, fmt.Errorf("dial proxy %s: %w", addr, err)
	}
	if dl, ok := ctx.Deadline(); ok {
		_ = up.SetDeadline(dl)
	}
	target := net.JoinHostPort(name, fmt.Sprint(port))
	req := "CONNECT " + target + " HTTP/1.1\r\nHost: " + target + "\r\n"
	if u := r.Proxy.User; u != nil {
		pw, _ := u.Password()
		req += "Proxy-Authorization: Basic " + base64.StdEncoding.EncodeToString([]byte(u.Username()+":"+pw)) + "\r\n"
	}
	req += "\r\n"
	if _, err := io.WriteString(up, req); err != nil {
		up.Close()
		return nil, nil, fmt.Errorf("write CONNECT: %w", err)
	}
	br := bufio.NewReader(up)
	resp, err := http.ReadResponse(br, &http.Request{Method: http.MethodConnect})
	if err != nil {
		up.Close()
		return nil, nil, fmt.Errorf("read CONNECT answer: %w", err)
	}
	if resp.StatusCode != http.StatusOK {
		up.Close()
		return nil, nil, fmt.Errorf("proxy answered %s to CONNECT %s", resp.Status, target)
	}
	_ = up.SetDeadline(time.Time{})
	var rest []byte
	if n := br.Buffered(); n > 0 {
		rest, _ = br.Peek(n)
		rest = append([]byte(nil), rest...)
	}
	return up, rest, nil
}

func splice(a, b net.Conn) {
	var wg sync.WaitGroup
	cp := func(dst, src net.Conn) {
		defer wg.Done()
		_, _ = io.Copy(dst, src)
		if cw, ok := dst.(interface{ CloseWrite() error }); ok {
			_ = cw.CloseWrite()
		} else {
			_ = dst.Close()
		}
	}
	wg.Add(2)
	go cp(a, b)
	go cp(b, a)
	wg.Wait()
}

// ServerName reads a TLS ClientHello from c and returns its server name and the bytes read (to send on unchanged).
func ServerName(c net.Conn) (string, []byte, error) {
	var buf bytes.Buffer
	var name string
	err := tls.Server(readOnly{r: io.TeeReader(c, &buf)}, &tls.Config{
		GetConfigForClient: func(h *tls.ClientHelloInfo) (*tls.Config, error) {
			name = h.ServerName
			return nil, errHelloRead
		},
	}).Handshake()
	if name == "" {
		if err == nil || errors.Is(err, errHelloRead) {
			err = errors.New("the client hello names no server")
		}
		return "", nil, err
	}
	return name, buf.Bytes(), nil
}

// readOnly is a net.Conn that only reads: the TLS server's alert after errHelloRead goes nowhere.
type readOnly struct {
	r io.Reader
	net.Conn
}

func (c readOnly) Read(p []byte) (int, error)     { return c.r.Read(p) }
func (readOnly) Write(p []byte) (int, error)      { return 0, io.ErrClosedPipe }
func (readOnly) Close() error                     { return nil }
func (readOnly) LocalAddr() net.Addr              { return &net.TCPAddr{} }
func (readOnly) RemoteAddr() net.Addr             { return &net.TCPAddr{} }
func (readOnly) SetDeadline(time.Time) error      { return nil }
func (readOnly) SetReadDeadline(time.Time) error  { return nil }
func (readOnly) SetWriteDeadline(time.Time) error { return nil }
