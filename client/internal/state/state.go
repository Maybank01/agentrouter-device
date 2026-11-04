// Package state holds what the client keeps on disk, all in one folder of the person's own profile (no admin rights,
// nothing outside it): the endpoints (config.json), the enrolment (device.json), the device credential, the SSH host
// key and NetBird's own config. Secrets are files only this account can read (0600; on Windows the profile folder's
// ACL), never command lines or environment variables (R21).
package state

import (
	"crypto/ed25519"
	"crypto/rand"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"time"

	"golang.org/x/crypto/ssh"
)

// Defaults set at release time (-ldflags "-X …state.DefaultIssuer=… -X …state.DefaultComputeURL=…"); empty in a
// developer build, which then needs config.json.
var (
	DefaultIssuer     = ""
	DefaultComputeURL = ""
)

// Dir is the client's folder.
type Dir string

// DefaultDir is <user config dir>/AgentDevice (%AppData%\AgentDevice, ~/Library/Application Support/AgentDevice,
// ~/.config/agentdevice).
func DefaultDir() (Dir, error) {
	base, err := os.UserConfigDir()
	if err != nil {
		return "", fmt.Errorf("state: no user config folder: %w", err)
	}
	name := "AgentDevice"
	if runtime.GOOS == "linux" || runtime.GOOS == "freebsd" {
		name = "agentdevice"
	}
	return Dir(filepath.Join(base, name)), nil
}

// Path returns a file in the folder.
func (d Dir) Path(name string) string { return filepath.Join(string(d), name) }

// Ensure creates the folder (0700).
func (d Dir) Ensure() error {
	if err := os.MkdirAll(string(d), 0o700); err != nil {
		return fmt.Errorf("state: create %s: %w", d, err)
	}
	return nil
}

// Config is config.json: where the services are. Every field may be left out and then has its default.
type Config struct {
	// Issuer is NewAPI's base URL, the tokens' iss.
	Issuer string `json:"issuer,omitempty"`
	// JWKSURL is the issuer's key set; default <issuer>/.well-known/jwks.json.
	JWKSURL string `json:"jwksUrl,omitempty"`
	// ComputeURL is AgentCompute's compute/v1 base URL.
	ComputeURL string `json:"computeUrl,omitempty"`
	// DeviceAuthURL and DeviceTokenURL override the device-code login endpoints.
	DeviceAuthURL  string `json:"deviceAuthUrl,omitempty"`
	DeviceTokenURL string `json:"deviceTokenUrl,omitempty"`
	// SSHPort is where the SSH server listens on the device's NetBird address (default 22; compute/v1 access target).
	SSHPort int `json:"sshPort,omitempty"`
	// TokenMaxAgeSeconds is how old (now - iat) an SSH token may be when it authenticates (default 600, compute/v1
	// access tokenMaxAgeSeconds).
	TokenMaxAgeSeconds int `json:"tokenMaxAgeSeconds,omitempty"`
}

// LoadConfig reads config.json (missing = defaults) and fills in the defaults.
func (d Dir) LoadConfig() (Config, error) {
	var c Config
	raw, err := os.ReadFile(d.Path("config.json"))
	switch {
	case errors.Is(err, os.ErrNotExist):
	case err != nil:
		return c, fmt.Errorf("state: read %s: %w", d.Path("config.json"), err)
	default:
		if err := json.Unmarshal(raw, &c); err != nil {
			return c, fmt.Errorf("state: %s is not valid JSON: %w", d.Path("config.json"), err)
		}
	}
	if c.Issuer == "" {
		c.Issuer = DefaultIssuer
	}
	if c.ComputeURL == "" {
		c.ComputeURL = DefaultComputeURL
	}
	c.Issuer = strings.TrimSuffix(c.Issuer, "/")
	c.ComputeURL = strings.TrimSuffix(c.ComputeURL, "/")
	if c.JWKSURL == "" && c.Issuer != "" {
		c.JWKSURL = c.Issuer + "/.well-known/jwks.json"
	}
	if c.SSHPort == 0 {
		c.SSHPort = 22
	}
	if c.TokenMaxAgeSeconds == 0 {
		c.TokenMaxAgeSeconds = 600
	}
	return c, nil
}

// Validate says what is missing for talking to the services.
func (c Config) Validate(path string) error {
	var missing []string
	if c.Issuer == "" {
		missing = append(missing, "issuer")
	}
	if c.ComputeURL == "" {
		missing = append(missing, "computeUrl")
	}
	if len(missing) > 0 {
		return fmt.Errorf("state: this build has no default %s; write them into %s (JSON: {\"issuer\": \"https://…\", \"computeUrl\": \"https://…\"})", strings.Join(missing, " and "), path)
	}
	return nil
}

// Device is device.json: the enrolment compute/v1 answered.
type Device struct {
	ComputerID    string    `json:"computerId"`
	Own           string    `json:"own"`
	Name          string    `json:"name"`
	ManagementURL string    `json:"managementUrl"`
	PeerName      string    `json:"peerName,omitempty"`
	JoinMethod    string    `json:"joinMethod"`
	Joined        bool      `json:"joined"`
	EnrolledAt    time.Time `json:"enrolledAt"`
}

// LoadDevice reads device.json; nil, nil when the device is not enrolled.
func (d Dir) LoadDevice() (*Device, error) {
	raw, err := os.ReadFile(d.Path("device.json"))
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("state: read %s: %w", d.Path("device.json"), err)
	}
	var dev Device
	if err := json.Unmarshal(raw, &dev); err != nil {
		return nil, fmt.Errorf("state: %s is not valid JSON: %w", d.Path("device.json"), err)
	}
	return &dev, nil
}

// SaveDevice writes device.json.
func (d Dir) SaveDevice(dev *Device) error {
	raw, _ := json.MarshalIndent(dev, "", "  ")
	return d.writePrivate("device.json", raw)
}

// Credential reads the device credential ("" when the person has not logged in).
func (d Dir) Credential() (string, error) {
	raw, err := os.ReadFile(d.Path("credential"))
	if errors.Is(err, os.ErrNotExist) {
		return "", nil
	}
	if err != nil {
		return "", fmt.Errorf("state: read the device credential %s: %w", d.Path("credential"), err)
	}
	return strings.TrimSpace(string(raw)), nil
}

// SaveCredential writes the device credential (0600).
func (d Dir) SaveCredential(cred string) error {
	return d.writePrivate("credential", []byte(cred+"\n"))
}

// NetBirdConfig is the path of NetBird's own config (WireGuard key, management URL).
func (d Dir) NetBirdConfig() string { return d.Path("netbird.json") }

// Forget removes the enrolment, the credential and NetBird's config; the host key and config.json stay.
func (d Dir) Forget() error {
	var errs []error
	for _, name := range []string{"device.json", "credential", "netbird.json", "idempotency-key"} {
		if err := os.Remove(d.Path(name)); err != nil && !errors.Is(err, os.ErrNotExist) {
			errs = append(errs, err)
		}
	}
	return errors.Join(errs...)
}

// IdempotencyKey returns the key of the pending enrolment, creating one; it is removed after the enrolment so a
// retried POST /v1/devices is the same enrolment.
func (d Dir) IdempotencyKey() (string, error) {
	if raw, err := os.ReadFile(d.Path("idempotency-key")); err == nil && len(strings.TrimSpace(string(raw))) > 0 {
		return strings.TrimSpace(string(raw)), nil
	}
	b := make([]byte, 16)
	_, _ = rand.Read(b)
	key := fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
	return key, d.writePrivate("idempotency-key", []byte(key))
}

// DoneEnrolling removes the idempotency key.
func (d Dir) DoneEnrolling() { _ = os.Remove(d.Path("idempotency-key")) }

// HostKey returns the SSH host key, generating an Ed25519 key at the first start (0600).
func (d Dir) HostKey() (ssh.Signer, []byte, error) {
	path := d.Path("ssh_host_ed25519_key")
	if raw, err := os.ReadFile(path); err == nil {
		signer, err := ssh.ParsePrivateKey(raw)
		if err != nil {
			return nil, nil, fmt.Errorf("state: host key %s is unreadable (remove it to make a new one, then enrol again): %w", path, err)
		}
		return signer, raw, nil
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, nil, fmt.Errorf("state: read host key %s: %w", path, err)
	}
	_, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		return nil, nil, fmt.Errorf("state: generate host key: %w", err)
	}
	block, err := ssh.MarshalPrivateKey(priv, "agentdevice host key")
	if err != nil {
		return nil, nil, fmt.Errorf("state: encode host key: %w", err)
	}
	raw := pem.EncodeToMemory(block)
	if err := d.writePrivate("ssh_host_ed25519_key", raw); err != nil {
		return nil, nil, err
	}
	signer, err := ssh.ParsePrivateKey(raw)
	if err != nil {
		return nil, nil, fmt.Errorf("state: parse new host key: %w", err)
	}
	return signer, raw, nil
}

// AuthorizedKey is the public key in OpenSSH authorized_keys format ("ssh-ed25519 AAAA…").
func AuthorizedKey(s ssh.Signer) string {
	return strings.TrimSpace(string(ssh.MarshalAuthorizedKey(s.PublicKey())))
}

// writePrivate writes a file only this account can read, atomically (temp file in the same folder, then rename).
func (d Dir) writePrivate(name string, data []byte) error {
	if err := d.Ensure(); err != nil {
		return err
	}
	tmp, err := os.CreateTemp(string(d), "."+name+".*")
	if err != nil {
		return fmt.Errorf("state: write %s: %w", d.Path(name), err)
	}
	defer os.Remove(tmp.Name())
	if err := tmp.Chmod(0o600); err != nil && runtime.GOOS != "windows" {
		tmp.Close()
		return fmt.Errorf("state: chmod %s: %w", d.Path(name), err)
	}
	if _, err := tmp.Write(data); err != nil {
		tmp.Close()
		return fmt.Errorf("state: write %s: %w", d.Path(name), err)
	}
	if err := tmp.Close(); err != nil {
		return fmt.Errorf("state: write %s: %w", d.Path(name), err)
	}
	if err := os.Rename(tmp.Name(), d.Path(name)); err != nil {
		return fmt.Errorf("state: replace %s: %w", d.Path(name), err)
	}
	return nil
}
