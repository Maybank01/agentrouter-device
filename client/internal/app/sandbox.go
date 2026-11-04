package app

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
	"net/url"
	"os"
	"sort"
	"strings"
	"time"

	"github.com/agentrouter-top/AgentDevice/client/internal/compute"
	"github.com/agentrouter-top/AgentDevice/client/internal/egress"
	"github.com/agentrouter-top/AgentDevice/client/internal/state"
)

// DefaultJoinFile is where AgentCompute puts a sandbox's join file (compute/v1 README §7.1).
const DefaultJoinFile = "/etc/agentcompute/network.json"

// SandboxJoin is compute/v1 sandbox-join.schema.json: what a sandbox's client needs to come online without a person.
type SandboxJoin struct {
	Version            int    `json:"version,omitempty"`
	Computer           string `json:"computer"`
	Own                string `json:"own"`
	Ten                string `json:"ten"`
	Issuer             string `json:"issuer"`
	ManagementURL      string `json:"managementUrl"`
	DeviceName         string `json:"deviceName"`
	SetupKey           string `json:"setupKey"`
	HostKey            string `json:"hostKey"`
	User               string `json:"user,omitempty"`
	TokenMaxAgeSeconds int    `json:"tokenMaxAgeSeconds,omitempty"`
	Proxy              *struct {
		URL    string   `json:"url"`
		Hosts  []string `json:"hosts"`
		Listen string   `json:"listen,omitempty"`
	} `json:"proxy,omitempty"`
}

// ReadSandboxJoin reads and checks the join file. The file holds secrets: errors never quote its values.
func ReadSandboxJoin(path string) (*SandboxJoin, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("sandbox: read the join file %s: %w", path, err)
	}
	var j SandboxJoin
	if err := json.Unmarshal(raw, &j); err != nil {
		return nil, fmt.Errorf("sandbox: the join file %s is not valid JSON (compute/v1 sandbox-join.schema.json): %w", path, err)
	}
	var missing []string
	for name, v := range map[string]string{"computer": j.Computer, "own": j.Own, "ten": j.Ten, "issuer": j.Issuer, "managementUrl": j.ManagementURL, "setupKey": j.SetupKey, "hostKey": j.HostKey} {
		if v == "" {
			missing = append(missing, name)
		}
	}
	if len(missing) > 0 {
		sort.Strings(missing)
		return nil, fmt.Errorf("sandbox: the join file %s lacks %s (an AgentCompute older than compute/v1 §7.1 wrote it; create the sandbox again)", path, strings.Join(missing, ", "))
	}
	if j.Proxy != nil {
		if _, err := url.Parse(j.Proxy.URL); err != nil || j.Proxy.URL == "" || len(j.Proxy.Hosts) == 0 {
			return nil, fmt.Errorf("sandbox: the join file %s has a proxy without a usable url and hosts", path)
		}
	}
	if j.DeviceName == "" {
		j.DeviceName = j.Computer
	}
	return &j, nil
}

// PrepareSandbox writes the join file's identity into the client's folder: the host key, config.json's issuer and
// token age, and device.json (a sandbox is enrolled by AgentCompute when it is created). It returns the join
// credential (the one-use setup key).
func (a *App) PrepareSandbox(j *SandboxJoin) (*compute.Join, error) {
	if err := a.Dir.SetHostKey([]byte(j.HostKey)); err != nil {
		return nil, err
	}
	a.Cfg.Issuer = strings.TrimSuffix(j.Issuer, "/")
	a.Cfg.JWKSURL = a.Cfg.Issuer + "/.well-known/jwks.json"
	if j.TokenMaxAgeSeconds > 0 {
		a.Cfg.TokenMaxAgeSeconds = j.TokenMaxAgeSeconds
	}
	dev, err := a.Dir.LoadDevice()
	if err != nil {
		return nil, err
	}
	if dev != nil && (dev.ComputerID != j.Computer || dev.ManagementURL != j.ManagementURL) {
		// Another sandbox's state (an image or disk reused): start over with this join file.
		a.Log.Warn("sandbox: the saved enrolment is another computer's; joining as the one the join file names", "saved", dev.ComputerID, "join", j.Computer)
		if err := a.Dir.Forget(); err != nil {
			return nil, err
		}
		dev = nil
	}
	if dev == nil {
		dev = &state.Device{ComputerID: j.Computer, Own: j.Own, Ten: j.Ten, Name: j.DeviceName, ManagementURL: j.ManagementURL,
			PeerName: j.DeviceName, JoinMethod: "setup-key", EnrolledAt: time.Now().UTC()}
		if err := a.Dir.SaveDevice(dev); err != nil {
			return nil, err
		}
	}
	a.Headless = true
	// Also after the first join: NetBird logs a known peer key in without it, and a start that died before
	// device.json said "joined" still has its key.
	return &compute.Join{Method: "setup-key", SetupKey: j.SetupKey}, nil
}

// RunSandbox keeps a sandbox online: the egress relay when the join file names a proxy, then Run.
func (a *App) RunSandbox(ctx context.Context, j *SandboxJoin, ui UI) error {
	join, err := a.PrepareSandbox(j)
	if err != nil {
		return err
	}
	if j.Proxy != nil {
		proxy, _ := url.Parse(j.Proxy.URL)
		// The client's own HTTPS (the issuer's key set) goes through the proxy too; AgentCompute's start normally
		// passes the node's proxy variables, which SSH sessions inherit.
		if os.Getenv("HTTPS_PROXY") == "" && os.Getenv("https_proxy") == "" {
			_ = os.Setenv("HTTPS_PROXY", j.Proxy.URL)
		}
		listen := j.Proxy.Listen
		if listen == "" {
			listen = "127.0.0.1:443"
		}
		ln, err := net.Listen("tcp", listen)
		if err != nil {
			return fmt.Errorf("sandbox: the egress relay cannot listen on %s (AgentCompute lets the work user bind 443 with net.ipv4.ip_unprivileged_port_start; is another client running?): %w", listen, err)
		}
		defer ln.Close()
		for _, h := range j.Proxy.Hosts {
			if ips, err := net.DefaultResolver.LookupHost(ctx, h); err == nil && !loopbackOnly(ips) {
				a.Log.Warn("sandbox: a NetBird host does not resolve to this computer, so its connections will not go through the relay", "host", h, "ips", strings.Join(ips, ","))
			}
		}
		relay := &egress.Relay{Proxy: proxy, Hosts: j.Proxy.Hosts, Log: a.Log.With("part", "egress")}
		go func() {
			if err := relay.Serve(ln); err != nil {
				a.Log.Error("sandbox: egress relay stopped", "error", err.Error())
			}
		}()
		a.Log.Info("sandbox: egress relay", "listen", listen, "proxy", proxy.Host, "hosts", strings.Join(j.Proxy.Hosts, ","))
	}
	return a.Run(ctx, join, ui)
}

func loopbackOnly(ips []string) bool {
	for _, s := range ips {
		if ip := net.ParseIP(s); ip == nil || !ip.IsLoopback() {
			return false
		}
	}
	return len(ips) > 0
}
