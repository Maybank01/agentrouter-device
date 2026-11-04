package compute

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/santhosh-tekuri/jsonschema/v6"

	"github.com/agentrouter-top/AgentDevice/client/internal/facts"
	"github.com/agentrouter-top/AgentDevice/client/internal/state"
)

var contracts = filepath.Join("..", "..", "testdata", "contracts")

// The vendored contract files are the providers' bytes: each matches its line in SHA256SUMS (C4).
func TestVendoredContractsMatchTheirSums(t *testing.T) {
	for _, provider := range []string{"agentcompute", "agentidentity"} {
		dir := filepath.Join(contracts, provider)
		f, err := os.Open(filepath.Join(dir, "SHA256SUMS"))
		if err != nil {
			t.Fatal(err)
		}
		n := 0
		s := bufio.NewScanner(f)
		for s.Scan() {
			sum, name, ok := strings.Cut(s.Text(), "  ")
			if !ok {
				continue
			}
			raw, err := os.ReadFile(filepath.Join(dir, filepath.FromSlash(name)))
			if err != nil {
				t.Fatal(err)
			}
			got := sha256.Sum256(raw)
			if hex.EncodeToString(got[:]) != sum {
				t.Errorf("%s/%s does not match SHA256SUMS: refresh it from the provider", provider, name)
			}
			n++
		}
		f.Close()
		if n < 2 {
			t.Errorf("%s/SHA256SUMS lists %d files", provider, n)
		}
	}
}

// schema compiles a definition of the vendored compute/v1 schemas. The files' $ids are URNs and their cross-file
// $refs are file names, so each file is registered under a file URL of its name (README "Schemas").
func schema(t *testing.T, file, pointer string) *jsonschema.Schema {
	t.Helper()
	dir := filepath.Join(contracts, "agentcompute", "contracts", "compute", "v1")
	c := jsonschema.NewCompiler()
	c.AssertFormat()
	entries, _ := os.ReadDir(dir)
	for _, e := range entries {
		raw, err := os.ReadFile(filepath.Join(dir, e.Name()))
		if err != nil {
			t.Fatal(err)
		}
		doc, err := jsonschema.UnmarshalJSON(bytes.NewReader(raw))
		if err != nil {
			t.Fatal(err)
		}
		delete(doc.(map[string]any), "$id")
		if err := c.AddResource("file:///compute-v1/"+e.Name(), doc); err != nil {
			t.Fatal(err)
		}
	}
	sch, err := c.Compile("file:///compute-v1/" + file + pointer)
	if err != nil {
		t.Fatal(err)
	}
	return sch
}

func validationError(sch *jsonschema.Schema, v any) error {
	raw, _ := json.Marshal(v)
	inst, err := jsonschema.UnmarshalJSON(bytes.NewReader(raw))
	if err != nil {
		return err
	}
	if err := sch.Validate(inst); err != nil {
		return fmt.Errorf("not the contract's shape: %v\n%s", err, raw)
	}
	return nil
}

func validate(t *testing.T, sch *jsonschema.Schema, v any) {
	t.Helper()
	if err := validationError(sch, v); err != nil {
		t.Fatal(err)
	}
}

// The facts this machine reports, as Collect gathers them on each CI runner OS, are valid compute/v1 facts.
func TestFactsFollowTheContract(t *testing.T) {
	f := facts.Collect(context.Background())
	validate(t, schema(t, "computer.schema.json", "#/$defs/facts"), f)
	if f.OS.Family == "" || f.Shell.Path == "" || f.Memory.TotalBytes == 0 {
		t.Fatalf("facts are missing basics: %+v", f)
	}
}

func testHostKey(t *testing.T) string {
	t.Helper()
	signer, _, err := state.Dir(t.TempDir()).HostKey()
	if err != nil {
		t.Fatal(err)
	}
	return state.AuthorizedKey(signer)
}

func TestEnrolSendsTheContractRequestAndReadsTheEnrolment(t *testing.T) {
	reqSchema := schema(t, "device.schema.json", "#/$defs/enrolRequest")
	answerSchema := schema(t, "device.schema.json", "#/$defs/enrolment")
	answer := map[string]any{
		"computer": map[string]any{
			"id": "cmp_test1", "kind": "device", "name": "台式机", "own": "acc_2002", "state": "starting",
			"facts": facts.Collect(context.Background()), "createdAt": "2026-11-04T10:00:00Z",
		},
		"network": map[string]any{
			"managementUrl": "https://netbird.test", "deviceName": "cmp_test1",
			"join": map[string]any{"method": "setup-key", "setupKey": "TEST-ONLY-setup-key"},
		},
	}
	validate(t, answerSchema, answer)
	var gotKey, gotAuth string
	var bodyErr error
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/v1/devices" {
			http.NotFound(w, r)
			return
		}
		gotKey, gotAuth = r.Header.Get("Idempotency-Key"), r.Header.Get("Authorization")
		var body map[string]any
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			w.WriteHeader(http.StatusBadRequest)
			return
		}
		if bodyErr = validationError(reqSchema, body); bodyErr != nil {
			w.WriteHeader(http.StatusUnprocessableEntity)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusCreated)
		_ = json.NewEncoder(w).Encode(answer)
	}))
	defer srv.Close()
	c := &Client{BaseURL: srv.URL}
	en, err := c.Enrol(context.Background(), "tok", "idem-1", EnrolRequest{
		Name: "台式机", Facts: facts.Collect(context.Background()), ClientVersion: "test", HostKey: testHostKey(t), User: "zero",
	})
	if bodyErr != nil {
		t.Fatal(bodyErr)
	}
	if err != nil {
		t.Fatal(err)
	}
	if gotKey != "idem-1" || gotAuth != "Bearer tok" {
		t.Fatalf("headers: Idempotency-Key %q, Authorization %q", gotKey, gotAuth)
	}
	if en.Computer.ID != "cmp_test1" || en.Computer.Own != "acc_2002" || en.Network.Join.SetupKey != "TEST-ONLY-setup-key" {
		t.Fatalf("enrolment %+v", en)
	}
}

func TestProblemsKeepTheirDetail(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/problem+json")
		w.WriteHeader(http.StatusForbidden)
		fmt.Fprint(w, `{"type":"urn:agentcompute:error:COMPUTE_SCOPE_MISSING","title":"Scope missing","status":403,"detail":"computer:cmp_x:report is not in scp","code":"COMPUTE_SCOPE_MISSING","component":"compute-api","operation":"reportFacts","target":"cmp_x","retryable":false,"next":"ask identity/v1 for a token with computer:cmp_x:report"}`)
	}))
	defer srv.Close()
	err := (&Client{BaseURL: srv.URL}).ReportFacts(context.Background(), "tok", "cmp_x", facts.Facts{})
	if err == nil {
		t.Fatal("want an error")
	}
	for _, want := range []string{"403", "COMPUTE_SCOPE_MISSING", "reportFacts", "cmp_x", "is not in scp", "next: ask identity/v1"} {
		if !strings.Contains(err.Error(), want) {
			t.Errorf("error lacks %q: %v", want, err)
		}
	}
}
