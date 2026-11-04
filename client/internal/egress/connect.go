package egress

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"time"
)

// Dial reaches host:port the way this computer's programs reach the internet: through the HTTP CONNECT proxy the
// environment names (HTTPS_PROXY / https_proxy, NO_PROXY respected), else directly. `agentdevice connect` uses it as an
// SSH ProxyCommand, so `ssh 台式机` in a sandbox reaches the compute SSH gateway through the node's proxy.
func Dial(ctx context.Context, host string, port int) (net.Conn, error) {
	target := net.JoinHostPort(host, fmt.Sprint(port))
	proxy, err := http.ProxyFromEnvironment(&http.Request{URL: &url.URL{Scheme: "https", Host: target}})
	if err != nil {
		return nil, fmt.Errorf("connect %s: the proxy variables are invalid: %w", target, err)
	}
	if proxy == nil {
		return (&net.Dialer{Timeout: 15 * time.Second}).DialContext(ctx, "tcp", target)
	}
	r := &Relay{Proxy: proxy, Port: port}
	conn, rest, err := r.connect(ctx, host)
	if err != nil {
		return nil, fmt.Errorf("connect %s through the proxy %s: %w", target, proxy.Host, err)
	}
	if len(rest) > 0 {
		return &prefixed{Conn: conn, rest: rest}, nil
	}
	return conn, nil
}

// prefixed returns bytes the proxy sent past its answer before the rest of the stream.
type prefixed struct {
	net.Conn
	rest []byte
}

func (p *prefixed) Read(b []byte) (int, error) {
	if len(p.rest) > 0 {
		n := copy(b, p.rest)
		p.rest = p.rest[n:]
		return n, nil
	}
	return p.Conn.Read(b)
}
