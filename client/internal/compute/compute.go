// Package compute is the client side of AgentCompute's compute/v1 "devices" operations (contracts/compute/v1:
// device.schema.json, openapi.yaml): enrol this computer, report its facts, unbind it. Every call carries an
// identity/v1 token with aud agentcompute; errors are the RFC 9457 problems of contracts/errors/v1, kept whole.
package compute

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"

	"github.com/agentrouter-top/AgentDevice/client/internal/facts"
)

// EnrolRequest is device.schema.json $defs/enrolRequest.
type EnrolRequest struct {
	Name          string      `json:"name"`
	Facts         facts.Facts `json:"facts"`
	ClientVersion string      `json:"clientVersion,omitempty"`
	// HostKey is the device's SSH host public key (authorized_keys format), which compute/v1 access answers as
	// hostKey; User is the account the client runs as, answered as user.
	HostKey string `json:"hostKey,omitempty"`
	User    string `json:"user,omitempty"`
}

// Computer is the part of computer.schema.json the client reads.
type Computer struct {
	ID    string `json:"id"`
	Kind  string `json:"kind"`
	Name  string `json:"name"`
	Own   string `json:"own"`
	State string `json:"state"`
}

// Join says how the embedded NetBird joins the owner's network.
type Join struct {
	Method   string `json:"method"` // setup-key | jwt
	SetupKey string `json:"setupKey,omitempty"`
}

// Network is device.schema.json enrolment.network.
type Network struct {
	ManagementURL string `json:"managementUrl"`
	DeviceName    string `json:"deviceName,omitempty"`
	Join          Join   `json:"join"`
}

// Enrolment is device.schema.json $defs/enrolment.
type Enrolment struct {
	Computer Computer `json:"computer"`
	Network  Network  `json:"network"`
}

// Problem is the errors/v1 envelope (RFC 9457 plus code, component, operation, target, retryable, next, cause).
type Problem struct {
	HTTPStatus int             `json:"-"`
	Type       string          `json:"type"`
	Title      string          `json:"title"`
	Status     int             `json:"status"`
	Detail     string          `json:"detail"`
	Code       string          `json:"code"`
	Component  string          `json:"component"`
	Operation  string          `json:"operation"`
	Target     string          `json:"target"`
	Retryable  bool            `json:"retryable"`
	Next       string          `json:"next"`
	User       string          `json:"user"`
	TraceID    string          `json:"traceId"`
	Cause      json.RawMessage `json:"cause,omitempty"`
}

func (p *Problem) Error() string {
	var b strings.Builder
	fmt.Fprintf(&b, "compute/v1 answered %d", p.HTTPStatus)
	if p.Code != "" {
		b.WriteString(" " + p.Code)
	}
	if p.Component != "" || p.Operation != "" {
		fmt.Fprintf(&b, " (%s %s", p.Component, p.Operation)
		if p.Target != "" {
			b.WriteString(" on " + p.Target)
		}
		b.WriteString(")")
	}
	if p.Detail != "" {
		b.WriteString(": " + p.Detail)
	} else if p.Title != "" {
		b.WriteString(": " + p.Title)
	}
	if len(p.Cause) > 0 {
		b.WriteString("; cause: " + string(p.Cause))
	}
	if p.Retryable {
		b.WriteString(" (retryable)")
	}
	if p.Next != "" {
		b.WriteString("; next: " + p.Next)
	}
	if p.TraceID != "" {
		b.WriteString(" [trace " + p.TraceID + "]")
	}
	return b.String()
}

// Client calls compute/v1 at BaseURL.
type Client struct {
	BaseURL string
	HTTP    *http.Client
}

func (c *Client) do(ctx context.Context, method, path, token string, headers map[string]string, in, out any) error {
	var body io.Reader
	if in != nil {
		raw, err := json.Marshal(in)
		if err != nil {
			return fmt.Errorf("compute/v1 %s %s: encode body: %w", method, path, err)
		}
		body = bytes.NewReader(raw)
	}
	url := strings.TrimSuffix(c.BaseURL, "/") + path
	req, err := http.NewRequestWithContext(ctx, method, url, body)
	if err != nil {
		return fmt.Errorf("compute/v1 %s %s: %w", method, url, err)
	}
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Accept", "application/json, application/problem+json")
	if in != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	for k, v := range headers {
		req.Header.Set(k, v)
	}
	client := c.HTTP
	if client == nil {
		client = &http.Client{Timeout: 60 * time.Second}
	}
	resp, err := client.Do(req)
	if err != nil {
		return fmt.Errorf("compute/v1 %s %s: %w (network; retry)", method, url, err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if resp.StatusCode >= 300 {
		p := &Problem{HTTPStatus: resp.StatusCode}
		if json.Unmarshal(raw, p) != nil || (p.Code == "" && p.Detail == "" && p.Title == "") {
			p.Detail = strings.TrimSpace(string(raw))
			if len(p.Detail) > 500 {
				p.Detail = p.Detail[:500] + "…"
			}
			p.Retryable = resp.StatusCode >= 500 || resp.StatusCode == 429
		}
		if p.Operation == "" {
			p.Operation = method + " " + path
		}
		return fmt.Errorf("compute/v1 %s %s: %w", method, url, p)
	}
	if out != nil {
		if err := json.Unmarshal(raw, out); err != nil {
			return fmt.Errorf("compute/v1 %s %s: the answer is not the contract's shape: %w", method, url, err)
		}
	}
	return nil
}

// Enrol is POST /v1/devices (scope device:*:enrol) with an Idempotency-Key, so a retry is the same enrolment.
func (c *Client) Enrol(ctx context.Context, token, idempotencyKey string, req EnrolRequest) (*Enrolment, error) {
	var out Enrolment
	if err := c.do(ctx, http.MethodPost, "/v1/devices", token, map[string]string{"Idempotency-Key": idempotencyKey}, req, &out); err != nil {
		return nil, err
	}
	if out.Computer.ID == "" || out.Network.ManagementURL == "" || out.Network.Join.Method == "" {
		return nil, fmt.Errorf("compute/v1 POST /v1/devices: the enrolment lacks computer.id, network.managementUrl or network.join")
	}
	if out.Network.Join.Method == "setup-key" && out.Network.Join.SetupKey == "" {
		return nil, fmt.Errorf("compute/v1 POST /v1/devices: join method setup-key without a setupKey")
	}
	return &out, nil
}

// ReportFacts is PUT /v1/computers/{id}/facts (scope computer:<id>:report).
func (c *Client) ReportFacts(ctx context.Context, token, id string, f facts.Facts) error {
	return c.do(ctx, http.MethodPut, "/v1/computers/"+id+"/facts", token, nil, f, nil)
}

// Unbind is DELETE /v1/devices/{id} (scope computer:<id>:manage): the NetBird peer is deleted.
func (c *Client) Unbind(ctx context.Context, token, id string) error {
	return c.do(ctx, http.MethodDelete, "/v1/devices/"+id, token, nil, nil, nil)
}
