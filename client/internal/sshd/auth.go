package sshd

import (
	"context"
	"fmt"
	"time"

	"github.com/agentrouter-top/AgentDevice/client/internal/identity"
)

// Authorizer decides whether an SSH password (an identity/v1 token) may open a session on this device:
// aud netbird, scp covering computer:<this id>:ssh, own equal to the device's owner, and issued at most MaxAge ago
// (compute/v1 access tokenMaxAgeSeconds). AgentCompute's SSH gateway checks the same token before forwarding; the
// device checks it again because it is the last hop.
type Authorizer struct {
	Verifier   *identity.Verifier
	ComputerID string
	Own        string
	MaxAge     time.Duration
}

// Audience is the identity/v1 audience of SSH tokens.
const Audience = "netbird"

// Authorize returns the token's claims, or an *identity.Error naming the failed check.
func (a *Authorizer) Authorize(ctx context.Context, token string) (*identity.Claims, error) {
	scope := "computer:" + a.ComputerID + ":ssh"
	c, err := a.Verifier.Verify(ctx, token, Audience, scope)
	if err != nil {
		return nil, err
	}
	if c.Own != a.Own {
		return nil, &identity.Error{Check: "own", Detail: fmt.Sprintf("own %s, this device belongs to %s", c.Own, a.Own)}
	}
	now := time.Now()
	if a.Verifier.Now != nil {
		now = a.Verifier.Now()
	}
	if age := now.Sub(time.Unix(c.Iat, 0)); a.MaxAge > 0 && age > a.MaxAge {
		return nil, &identity.Error{Check: "age", Detail: fmt.Sprintf("issued %s ago, at most %s when connecting: mint the token right before connecting", age.Truncate(time.Second), a.MaxAge)}
	}
	return c, nil
}
