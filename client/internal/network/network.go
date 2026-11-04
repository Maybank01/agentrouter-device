// Package network joins the owner's private NetBird network with NetBird's embedded client (client/embed v0.82.0,
// BSD-3) in userspace (netstack): no administrator rights, no driver, no inbound port on the computer's own network.
// NetBird is used for the network only; its SSH server stays off (docs/adr/0002-own-ssh-server.md) and the
// client's own SSH server listens on the NetBird address (internal/sshd).
package network

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/netbirdio/netbird/client/embed"
)

// Options says how to join.
type Options struct {
	// ConfigPath is NetBird's own config file (it keeps the peer's WireGuard key there).
	ConfigPath    string
	ManagementURL string
	DeviceName    string
	// SetupKey or JWT is the one-use join credential of the first start; later starts log in with the peer key
	// in ConfigPath. Kept in memory only.
	SetupKey string
	JWT      string
	// LogOutput receives NetBird's log.
	LogOutput io.Writer
	LogLevel  string
}

// Network is a joined NetBird client.
type Network struct {
	c *embed.Client
}

// Start joins (blocking until the engine runs or ctx ends).
func Start(ctx context.Context, o Options) (*Network, error) {
	skipSlowStaticInfo()
	privateKey, err := prepareConfig(o.ConfigPath)
	if err != nil {
		return nil, err
	}
	opts := embed.Options{
		DeviceName:          o.DeviceName,
		ManagementURL:       o.ManagementURL,
		ConfigPath:          o.ConfigPath,
		LogOutput:           o.LogOutput,
		LogLevel:            o.LogLevel,
		DisableClientRoutes: true,
	}
	switch {
	case o.SetupKey != "":
		opts.SetupKey = o.SetupKey
	case o.JWT != "":
		opts.JWTToken = o.JWT
	case privateKey != "":
		opts.PrivateKey = privateKey
	default:
		return nil, fmt.Errorf("network: %s has no peer key and no join credential was given: enrol this device again (agentdevice login)", o.ConfigPath)
	}
	c, err := embed.New(opts)
	if err != nil {
		return nil, fmt.Errorf("network: NetBird client (management %s): %w", o.ManagementURL, err)
	}
	if err := c.Start(ctx); err != nil {
		return nil, fmt.Errorf("network: join the NetBird network at %s: %w (retry; check that the management address is reachable)", o.ManagementURL, err)
	}
	return &Network{c: c}, nil
}

// prepareConfig keeps NetBird's own SSH server off in its config (a new profile already defaults to off; an older
// config without the field would default to on) and returns the peer's WireGuard key when it has joined before.
func prepareConfig(path string) (string, error) {
	cfg := map[string]any{}
	raw, err := os.ReadFile(path)
	switch {
	case errors.Is(err, os.ErrNotExist):
	case err != nil:
		return "", fmt.Errorf("network: read NetBird config %s: %w", path, err)
	default:
		if err := json.Unmarshal(raw, &cfg); err != nil {
			return "", fmt.Errorf("network: NetBird config %s is not JSON (remove it and enrol again): %w", path, err)
		}
	}
	cfg["ServerSSHAllowed"] = false
	cfg["EnableSSHSFTP"] = false
	cfg["EnableSSHLocalPortForwarding"] = false
	cfg["EnableSSHRemotePortForwarding"] = false
	cfg["EnableSSHRoot"] = false
	out, _ := json.MarshalIndent(cfg, "", "    ")
	if err := os.WriteFile(path, out, 0o600); err != nil {
		return "", fmt.Errorf("network: write NetBird config %s: %w", path, err)
	}
	key, _ := cfg["PrivateKey"].(string)
	return key, nil
}

// Listen listens on port of the device's NetBird address (netstack only: nothing opens on the computer's network).
func (n *Network) Listen(port int) (net.Listener, error) {
	ln, err := n.c.ListenTCP(net.JoinHostPort("0.0.0.0", strconv.Itoa(port)))
	if err != nil {
		return nil, fmt.Errorf("network: listen on the NetBird address, port %d: %w", port, err)
	}
	return ln, nil
}

// State is what the tray and status show.
type State struct {
	Connected bool
	IP        string
	FQDN      string
	Peers     int
	Error     string
}

// Status reads the client's state.
func (n *Network) Status() State {
	st, err := n.c.Status()
	if err != nil {
		return State{Error: err.Error()}
	}
	s := State{
		Connected: st.ManagementState.Connected,
		IP:        strings.Split(st.LocalPeerState.IP, "/")[0],
		FQDN:      st.LocalPeerState.FQDN,
		Peers:     len(st.Peers),
	}
	if st.ManagementState.Error != nil {
		s.Error = st.ManagementState.Error.Error()
	}
	return s
}

// Stop leaves the network; bounded so closing the console window (Windows allows about 5 s) still ends cleanly.
func (n *Network) Stop() error {
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	return n.c.Stop(ctx)
}
