package state

import (
	"os"
	"strings"
	"testing"
)

func TestConfigSelectsTheZitadelLogin(t *testing.T) {
	d := Dir(t.TempDir())
	write := func(s string) {
		t.Helper()
		if err := os.WriteFile(d.Path("config.json"), []byte(s), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	// No zitadel: the NewAPI device login, no exchange endpoint.
	write(`{"issuer":"https://newapi.test/","computeUrl":"https://compute.test"}`)
	c, err := d.LoadConfig()
	if err != nil || c.Zitadel != nil || c.ExchangeURL != "" || c.Validate("config.json") != nil {
		t.Fatalf("legacy config %+v, %v", c, err)
	}
	// zitadel: the exchange defaults to the issuer (the STS); the JWKS stays the issuer's.
	write(`{"issuer":"https://sts.test/","computeUrl":"https://compute.test","zitadel":{"url":"https://login.test/","projectId":"1","clientId":"2"}}`)
	c, err = d.LoadConfig()
	if err != nil || c.Validate("config.json") != nil {
		t.Fatalf("zitadel config: %v / %v", err, c.Validate("config.json"))
	}
	if c.Zitadel.URL != "https://login.test" || c.ExchangeURL != "https://sts.test/v1/identity/exchange" || c.JWKSURL != "https://sts.test/.well-known/jwks.json" {
		t.Fatalf("zitadel config %+v %+v", c, *c.Zitadel)
	}
	write(`{"issuer":"https://sts.test","computeUrl":"https://compute.test","zitadel":{"url":"https://login.test"}}`)
	c, _ = d.LoadConfig()
	if err := c.Validate("config.json"); err == nil || !strings.Contains(err.Error(), "zitadel.clientId, zitadel.projectId") {
		t.Fatalf("want the missing ids named, got %v", err)
	}
}
