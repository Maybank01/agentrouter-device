package app

import (
	"bytes"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/santhosh-tekuri/jsonschema/v6"
	"golang.org/x/crypto/ssh"

	"github.com/agentrouter-top/AgentDevice/client/internal/state"
)

// joinFile is a join file as AgentCompute writes it (src/service/sandbox-join.ts: PKCS#8 PEM host key).
func joinFile(t *testing.T) (map[string]any, ed25519.PublicKey) {
	t.Helper()
	pub, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	der, err := x509.MarshalPKCS8PrivateKey(priv)
	if err != nil {
		t.Fatal(err)
	}
	return map[string]any{
		"version": 1, "computer": "cmp_sandbox0test0id0aa", "own": "acc_7", "ten": "acc_2210",
		"issuer": "https://newapi.test", "managementUrl": "https://netbird.test", "deviceName": "cmp_sandbox0test0id0aa",
		"setupKey": "TEST-SETUP-KEY", "hostKey": string(pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: der})),
		"user": "node", "tokenMaxAgeSeconds": 600,
		"proxy": map[string]any{"url": "http://172.20.0.1:3128", "hosts": []string{"netbird.test"}},
	}, pub
}

func joinSchema(t *testing.T) *jsonschema.Schema {
	t.Helper()
	raw, err := os.ReadFile(filepath.Join("..", "..", "testdata", "contracts", "agentcompute", "contracts", "compute", "v1", "sandbox-join.schema.json"))
	if err != nil {
		t.Fatal(err)
	}
	doc, err := jsonschema.UnmarshalJSON(bytes.NewReader(raw))
	if err != nil {
		t.Fatal(err)
	}
	delete(doc.(map[string]any), "$id")
	c := jsonschema.NewCompiler()
	c.AssertFormat()
	if err := c.AddResource("file:///compute-v1/sandbox-join.schema.json", doc); err != nil {
		t.Fatal(err)
	}
	sch, err := c.Compile("file:///compute-v1/sandbox-join.schema.json")
	if err != nil {
		t.Fatal(err)
	}
	return sch
}

func writeJoin(t *testing.T, v any) string {
	t.Helper()
	raw, _ := json.Marshal(v)
	path := filepath.Join(t.TempDir(), "network.json")
	if err := os.WriteFile(path, raw, 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

// The provider's file shape (the vendored schema) is what ReadSandboxJoin takes, and PrepareSandbox makes the
// computer the file names: its host key, owner, tenant, issuer and the one-use setup key.
func TestSandboxJoinFromTheProvidersFile(t *testing.T) {
	file, pub := joinFile(t)
	raw, _ := json.Marshal(file)
	inst, _ := jsonschema.UnmarshalJSON(bytes.NewReader(raw))
	if err := joinSchema(t).Validate(inst); err != nil {
		t.Fatalf("the test file is not the contract's shape: %v", err)
	}
	j, err := ReadSandboxJoin(writeJoin(t, file))
	if err != nil {
		t.Fatal(err)
	}
	dir := state.Dir(t.TempDir())
	cfg, _ := dir.LoadConfig()
	a := &App{Dir: dir, Cfg: cfg, Log: slog.New(slog.NewTextHandler(io.Discard, nil))}
	join, err := a.PrepareSandbox(j)
	if err != nil {
		t.Fatal(err)
	}
	if !a.Headless || join == nil || join.Method != "setup-key" || join.SetupKey != "TEST-SETUP-KEY" {
		t.Fatalf("join %+v headless %v", join, a.Headless)
	}
	if a.Cfg.Issuer != "https://newapi.test" || a.Cfg.JWKSURL != "https://newapi.test/.well-known/jwks.json" || a.Cfg.TokenMaxAgeSeconds != 600 {
		t.Fatalf("config %+v", a.Cfg)
	}
	dev, _ := dir.LoadDevice()
	if dev == nil || dev.ComputerID != "cmp_sandbox0test0id0aa" || dev.Own != "acc_7" || dev.Ten != "acc_2210" || dev.PeerName != dev.ComputerID || dev.JoinMethod != "setup-key" {
		t.Fatalf("device %+v", dev)
	}
	signer, _, err := dir.HostKey()
	if err != nil {
		t.Fatal(err)
	}
	want, _ := ssh.NewPublicKey(pub)
	if !bytes.Equal(signer.PublicKey().Marshal(), want.Marshal()) {
		t.Fatal("the SSH host key is not the one AgentCompute generated")
	}

	// Started again (after :start): the same computer keeps its state; another computer's state is replaced.
	dev.Joined = true
	_ = dir.SaveDevice(dev)
	if _, err := a.PrepareSandbox(j); err != nil {
		t.Fatal(err)
	}
	if d, _ := dir.LoadDevice(); !d.Joined {
		t.Fatal("a restart forgot the first join")
	}
	j.Computer = "cmp_another0sandbox0id0"
	if _, err := a.PrepareSandbox(j); err != nil {
		t.Fatal(err)
	}
	if d, _ := dir.LoadDevice(); d.ComputerID != "cmp_another0sandbox0id0" || d.Joined {
		t.Fatalf("another computer's state was kept: %+v", d)
	}
}

// A file of the first AgentCompute (managementUrl, deviceName, setupKey only) is refused naming what is missing,
// never quoting the secret values.
func TestSandboxJoinRefusesAnOldFile(t *testing.T) {
	_, err := ReadSandboxJoin(writeJoin(t, map[string]any{"managementUrl": "https://netbird.test", "deviceName": "cmp_x", "setupKey": "SECRET-KEY"}))
	if err == nil {
		t.Fatal("an old join file was accepted")
	}
	for _, want := range []string{"computer", "own", "issuer", "hostKey", "create the sandbox again"} {
		if !strings.Contains(err.Error(), want) {
			t.Errorf("%q lacks %q", err, want)
		}
	}
	if strings.Contains(err.Error(), "SECRET-KEY") {
		t.Fatal("the error quotes the setup key")
	}
}
