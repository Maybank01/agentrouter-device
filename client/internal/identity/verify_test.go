package identity

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"
)

// The provider's vectors, vendored byte for byte (testdata/contracts/agentidentity, SHA256SUMS there).
var vectorDir = filepath.Join("..", "..", "testdata", "contracts", "agentidentity", "contracts", "identity", "v1", "vectors")

func loadKeys(t *testing.T) StaticKeys {
	t.Helper()
	raw, err := os.ReadFile(filepath.Join(vectorDir, "TEST-ONLY-jwks.json"))
	if err != nil {
		t.Fatal(err)
	}
	keys, err := ParseJWKS(raw)
	if err != nil {
		t.Fatal(err)
	}
	return keys
}

func TestContractVectors(t *testing.T) {
	raw, err := os.ReadFile(filepath.Join(vectorDir, "tokens.json"))
	if err != nil {
		t.Fatal(err)
	}
	var file struct {
		Iss   string `json:"iss"`
		Now   int64  `json:"now"`
		Cases []struct {
			Name   string `json:"name"`
			Token  string `json:"token"`
			Verify struct {
				Aud   string `json:"aud"`
				Scope string `json:"scope"`
				Now   int64  `json:"now"`
			} `json:"verify"`
			Valid bool   `json:"valid"`
			Error string `json:"error"`
		} `json:"cases"`
	}
	if err := json.Unmarshal(raw, &file); err != nil {
		t.Fatal(err)
	}
	if len(file.Cases) < 20 {
		t.Fatalf("only %d vectors", len(file.Cases))
	}
	keys := loadKeys(t)
	for _, c := range file.Cases {
		t.Run(c.Name, func(t *testing.T) {
			now := file.Now
			if c.Verify.Now != 0 {
				now = c.Verify.Now
			}
			v := &Verifier{Issuer: file.Iss, Keys: keys, Now: func() time.Time { return time.Unix(now, 0) }}
			_, err := v.Verify(context.Background(), c.Token, c.Verify.Aud, c.Verify.Scope)
			if c.Valid {
				if err != nil {
					t.Fatalf("want valid, got %v", err)
				}
				return
			}
			if err == nil {
				t.Fatalf("want rejected (%s), got valid", c.Error)
			}
			if got := Check(err); got != c.Error {
				t.Fatalf("want check %q, got %q (%v)", c.Error, got, err)
			}
		})
	}
}

func TestCovers(t *testing.T) {
	cases := []struct {
		held []string
		want string
		ok   bool
	}{
		{[]string{"computer:cmp_a:ssh"}, "computer:cmp_a:ssh", true},
		{[]string{"computer:*:ssh"}, "computer:cmp_a:ssh", true},
		{[]string{"computer:cmp_b:ssh"}, "computer:cmp_a:ssh", false},
		{[]string{"computer:cmp_a:read"}, "computer:cmp_a:ssh", false},
		{[]string{"space:*:rw"}, "space:spc_x:ro", true},
		{[]string{"space:*:rw"}, "space:new:create", false},
		{[]string{"space:spc_x:ro"}, "space:spc_x:rw", false},
		{[]string{"bad"}, "computer:cmp_a:ssh", false},
	}
	for _, c := range cases {
		if got := Covers(c.held, c.want); got != c.ok {
			t.Errorf("Covers(%v, %s) = %v, want %v", c.held, c.want, got, c.ok)
		}
	}
}

func TestRemoteKeysCachesAndRefetchesOnUnknownKid(t *testing.T) {
	raw, err := os.ReadFile(filepath.Join(vectorDir, "TEST-ONLY-jwks.json"))
	if err != nil {
		t.Fatal(err)
	}
	var hits atomic.Int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		w.Header().Set("Cache-Control", "public, max-age=600")
		_, _ = w.Write(raw)
	}))
	defer srv.Close()
	r := &RemoteKeys{URL: srv.URL + "/.well-known/jwks.json"}
	ctx := context.Background()
	if _, err := r.Key(ctx, "test-2026-11-a"); err != nil {
		t.Fatal(err)
	}
	if _, err := r.Key(ctx, "test-2026-11-b"); err != nil {
		t.Fatal(err)
	}
	if hits.Load() != 1 {
		t.Fatalf("fetched %d times, want 1 (cached)", hits.Load())
	}
	if _, err := r.Key(ctx, "nope"); err == nil {
		t.Fatal("unknown kid accepted")
	}
	if _, err := r.Key(ctx, "nope"); err == nil {
		t.Fatal("unknown kid accepted")
	}
	if hits.Load() != 1 {
		t.Fatalf("an unknown kid refetched within %s (%d fetches)", RefetchInterval, hits.Load())
	}
}

// The account pattern is the contract's (identity/v1 1.1.0, AgentIdentity#42): the regexes equal the vendored
// claims.schema.json, and the custom ZITADEL ids of the September migration are accounts.
func TestAccountPatternIsTheContracts(t *testing.T) {
	raw, err := os.ReadFile(filepath.Join(vectorDir, "..", "claims.schema.json"))
	if err != nil {
		t.Fatal(err)
	}
	var schema struct {
		Properties struct {
			Sub struct {
				Pattern string `json:"pattern"`
			} `json:"sub"`
		} `json:"properties"`
		Defs struct {
			Account struct {
				Pattern string `json:"pattern"`
			} `json:"account"`
		} `json:"$defs"`
	}
	if err := json.Unmarshal(raw, &schema); err != nil {
		t.Fatal(err)
	}
	if got := accountPattern.String(); got != schema.Defs.Account.Pattern {
		t.Fatalf("accountPattern %s, the contract's $defs/account is %s", got, schema.Defs.Account.Pattern)
	}
	if got := subPattern.String(); got != schema.Properties.Sub.Pattern {
		t.Fatalf("subPattern %s, the contract's sub is %s", got, schema.Properties.Sub.Pattern)
	}
	for id, ok := range map[string]bool{"acc_398174613395214436": true, "acc_ar3-legacy-user-1042": true,
		"acc_account-browser-0123456789abcdef0123456789abcdef": true, "acc_a_b": true,
		"acc_ar3:legacy": false, "acc_a/b": false, "acc_a.b": false, "acc_a b": false, "acc_": false} {
		if accountPattern.MatchString(id) != ok || subPattern.MatchString(id) != ok {
			t.Errorf("%q: want account %v", id, ok)
		}
	}
}
