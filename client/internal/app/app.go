// Package app is the client's flow: log the person in (device code), enrol the computer with compute/v1, join the
// owner's NetBird network, serve SSH on the NetBird address, report facts, show the state in the tray.
package app

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"os"
	"os/user"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/agentrouter-top/AgentDevice/client/internal/compute"
	"github.com/agentrouter-top/AgentDevice/client/internal/facts"
	"github.com/agentrouter-top/AgentDevice/client/internal/identity"
	"github.com/agentrouter-top/AgentDevice/client/internal/network"
	"github.com/agentrouter-top/AgentDevice/client/internal/sshd"
	"github.com/agentrouter-top/AgentDevice/client/internal/state"
	"github.com/agentrouter-top/AgentDevice/client/internal/tray"
	"github.com/agentrouter-top/AgentDevice/client/internal/update"
)

// UI is where the client shows the person what happens (the tray, or the terminal).
type UI interface {
	// Prompt shows the device-code login: open the address, confirm the code.
	Prompt(identity.Prompt)
	// Status shows the device's state in words.
	Status(s tray.State, text string)
}

// App holds the client's setup.
type App struct {
	Dir     state.Dir
	Cfg     state.Config
	Log     *slog.Logger
	LogFile io.Writer
	Version string
	// Self is this program's path (for the login-environment probe).
	Self string
	// BaseEnv is the process environment as it was before NetBird changed it.
	BaseEnv []string
	// Console is the visible activity log (Windows: the one console window); nil = none.
	Console io.Writer
	// Headless is a compute/v1 sandbox (RunSandbox): no device login, so no facts report and no issuer to ask
	// whether the device was removed.
	Headless bool

	sessOnce sync.Once
	sess     *identity.Session
}

func (a *App) computeClient() *compute.Client { return &compute.Client{BaseURL: a.Cfg.ComputeURL} }

// session is the device login (identity/v1 "Device login"): one per process, so refreshes happen one at a time.
// config.json's zitadel selects the login at ZITADEL with exchanges at the STS; without it, NewAPI's device login.
func (a *App) session() *identity.Session {
	a.sessOnce.Do(func() {
		var ep identity.Endpoints
		if z := a.Cfg.Zitadel; z != nil {
			ep = identity.ZitadelEndpoints(z.URL, z.ProjectID, z.ClientID, a.Cfg.ExchangeURL)
		} else {
			ep = identity.EndpointsFor(a.Cfg.Issuer)
			if a.Cfg.ClientID != "" {
				ep.ClientID = a.Cfg.ClientID
			}
		}
		if a.Cfg.DeviceAuthURL != "" {
			ep.DeviceAuthURL = a.Cfg.DeviceAuthURL
		}
		if a.Cfg.DeviceTokenURL != "" {
			ep.TokenURL = a.Cfg.DeviceTokenURL
		}
		a.sess = &identity.Session{Endpoints: ep, Store: a.Dir}
	})
	return a.sess
}

// token is a short identity/v1 token for aud (agentcompute or netbird), from the refresh token.
func (a *App) token(ctx context.Context, aud string) (string, error) {
	return a.session().Token(ctx, aud)
}

// Login runs the device-code login and enrols the computer under name. It returns the one-use join credential
// (kept in memory only) for the first Run.
func (a *App) Login(ctx context.Context, name string, ui UI) (*compute.Join, error) {
	if err := a.Cfg.Validate(a.Dir.Path("config.json")); err != nil {
		return nil, err
	}
	ui.Status(tray.Connecting, "等待登录")
	g, err := a.session().Login(ctx, name, func(p identity.Prompt) {
		// The log names the confirmation address too (support, and the hosted-runner end-to-end check, #231): the code
		// is what the window shows anyway, valid only for minutes and only for this login.
		a.Log.Info("device login: waiting for the confirmation in a browser", "url", p.VerificationURIComplete, "code", p.UserCode, "until", p.Expiry.UTC().Format(time.RFC3339))
		ui.Prompt(p)
	})
	if err != nil {
		return nil, err
	}
	ten := ""
	if c, err := identity.UnverifiedClaims(g.AccessToken); err == nil {
		ten = c.Ten
	}
	a.Log.Info("logged in", "issuer", a.Cfg.Issuer, "login", a.session().Endpoints.TokenURL, "device", g.DeviceID, "ten", ten)
	return a.enrol(ctx, name, ten, g.DeviceID)
}

// enrol is POST /v1/devices for this computer with its facts, host key and account.
func (a *App) enrol(ctx context.Context, name, ten, deviceID string) (*compute.Join, error) {
	signer, _, err := a.Dir.HostKey()
	if err != nil {
		return nil, err
	}
	tok, err := a.token(ctx, identity.AudienceCompute)
	if err != nil {
		return nil, fmt.Errorf("enrol: %w", err)
	}
	key, err := a.Dir.IdempotencyKey()
	if err != nil {
		return nil, err
	}
	account := AccountName()
	if account == "" {
		// compute/v1 would enrol a computer nobody can reach (every /access refused: no SSH user, AgentDevice#229).
		return nil, errors.New("enrol: this computer's account has no name (no user entry, USER and LOGNAME unset, no uid); nothing was enrolled. Run the client as a named account, or set USER, then log in again")
	}
	req := compute.EnrolRequest{
		Name:          name,
		Facts:         facts.Collect(ctx),
		ClientVersion: a.Version,
		HostKey:       state.AuthorizedKey(signer),
		User:          account,
	}
	en, err := a.computeClient().Enrol(ctx, tok, key, req)
	if err != nil {
		return nil, fmt.Errorf("enrol: %w", err)
	}
	dev := &state.Device{
		ComputerID:    en.Computer.ID,
		Own:           en.Computer.Own,
		Ten:           ten,
		DeviceID:      deviceID,
		Name:          en.Computer.Name,
		ManagementURL: en.Network.ManagementURL,
		PeerName:      en.Network.DeviceName,
		JoinMethod:    en.Network.Join.Method,
		EnrolledAt:    time.Now().UTC(),
	}
	if dev.Name == "" {
		dev.Name = name
	}
	if err := a.Dir.SaveDevice(dev); err != nil {
		return nil, err
	}
	a.Dir.DoneEnrolling()
	a.Log.Info("enrolled", "computer", dev.ComputerID, "own", dev.Own, "name", dev.Name, "join", dev.JoinMethod)
	return &en.Network.Join, nil
}

// AccountName is the account the client runs as, as SSH and compute/v1 access name it (on Windows without the
// computer prefix).
func AccountName() string {
	return accountName(user.Current, os.Getenv, os.Getuid)
}

// accountName is the OS account the client runs as, the `user` every SSH session runs as (compute/v1 device.schema.json):
// the account's name; without a passwd entry (a container's bare uid, AgentDevice#229) $USER, then $LOGNAME, then the
// numeric uid (this client's SSH server runs every session as its own account whatever name the caller sends, so the
// uid reaches it as well as a name). Empty only when none of them exists (Windows without a current user).
func accountName(current func() (*user.User, error), getenv func(string) string, getuid func() int) string {
	if u, err := current(); err == nil && u.Username != "" {
		name := u.Username
		if i := strings.LastIndexByte(name, '\\'); i >= 0 {
			name = name[i+1:]
		}
		return name
	}
	for _, key := range []string{"USER", "LOGNAME"} {
		if v := strings.TrimSpace(getenv(key)); v != "" {
			return v
		}
	}
	if uid := getuid(); uid >= 0 {
		return strconv.Itoa(uid)
	}
	return ""
}

// Unbind removes this computer at compute/v1 (its NetBird peer is deleted), ends the login at ZITADEL (its refresh
// token is revoked) and forgets the enrolment here.
func (a *App) Unbind(ctx context.Context) error {
	dev, err := a.Dir.LoadDevice()
	if err != nil {
		return err
	}
	if dev == nil {
		return errors.New("this device is not enrolled")
	}
	tok, err := a.token(ctx, identity.AudienceCompute)
	if err != nil {
		return fmt.Errorf("unbind: %w", err)
	}
	if err := a.computeClient().Unbind(ctx, tok, dev.ComputerID); err != nil {
		return fmt.Errorf("unbind: %w", err)
	}
	if err := a.session().Revoke(ctx); err != nil {
		// The computer is gone at compute/v1 already; the refresh token is forgotten below and ends at ZITADEL's expiry.
		a.Log.Warn("unbind: the login could not be revoked at ZITADEL", "error", err.Error())
	}
	return a.Dir.Forget()
}

// joinOptions are the NetBird options of this device; join is the one-use credential of the first start (nil later).
func (a *App) joinOptions(ctx context.Context, dev *state.Device, join *compute.Join) (network.Options, error) {
	o := network.Options{
		ConfigPath:    a.Dir.NetBirdConfig(),
		ManagementURL: dev.ManagementURL,
		DeviceName:    dev.PeerName,
		LogOutput:     a.LogFile,
		LogLevel:      "warn",
	}
	if o.DeviceName == "" {
		o.DeviceName = dev.ComputerID
	}
	if dev.Joined || join == nil {
		if dev.JoinMethod == "jwt" && !dev.Joined {
			a.session().Fresh(identity.AudienceNetBird) // NetBird refuses a token older than 600 s
			tok, err := a.token(ctx, identity.AudienceNetBird)
			if err != nil {
				return o, err
			}
			o.JWT = tok
		}
		return o, nil
	}
	switch join.Method {
	case "setup-key":
		o.SetupKey = join.SetupKey
	case "jwt":
		a.session().Fresh(identity.AudienceNetBird)
		tok, err := a.token(ctx, identity.AudienceNetBird)
		if err != nil {
			return o, err
		}
		o.JWT = tok
	default:
		return o, fmt.Errorf("join: compute/v1 named an unknown join method %q", join.Method)
	}
	return o, nil
}

// Run keeps the device online until ctx ends: network, SSH server, facts, tray state. When the login is refused for
// good (the person removed the device, or it was revoked) it forgets the enrolment here and returns an error that
// wraps identity.ErrDeviceRemoved.
func (a *App) Run(parent context.Context, join *compute.Join, ui UI) error {
	ctx, stop := context.WithCancelCause(parent)
	defer stop(nil)
	err := a.run(ctx, stop, join, ui)
	if cause := context.Cause(ctx); errors.Is(cause, identity.ErrDeviceRemoved) {
		err = cause
	}
	if errors.Is(err, identity.ErrDeviceRemoved) {
		a.Log.Error("device removed or login revoked; forgetting the enrolment", "error", err.Error())
		if !a.Headless {
			// The login is of no use without the computer: end it at ZITADEL too (already refused when that was the cause).
			rctx, cancel := context.WithTimeout(context.WithoutCancel(parent), 10*time.Second)
			if rerr := a.session().Revoke(rctx); rerr != nil {
				a.Log.Info("the device login could not be ended at ZITADEL (it may be refused already)", "error", rerr.Error())
			}
			cancel()
		}
		if ferr := a.Dir.Forget(); ferr != nil {
			a.Log.Warn("could not forget the enrolment", "error", ferr.Error())
		}
		return err
	}
	return err
}

func (a *App) run(ctx context.Context, stop context.CancelCauseFunc, join *compute.Join, ui UI) error {
	dev, err := a.Dir.LoadDevice()
	if err != nil {
		return err
	}
	if dev == nil {
		return errors.New("this device is not enrolled: run `agentdevice login`")
	}
	if !dev.Joined && join == nil && dev.JoinMethod == "setup-key" {
		// The one-use setup key lived only in the enrolling process, which ended before the first join.
		return fmt.Errorf("the first join of %s (%s) did not finish and its one-use key is gone: run `agentdevice unbind`, then `agentdevice login` again", dev.Name, dev.ComputerID)
	}
	signer, _, err := a.Dir.HostKey()
	if err != nil {
		return err
	}
	sh := facts.UserShell()
	env := sshd.LoginEnv(ctx, sh, a.Self, a.BaseEnv)
	home, _ := os.UserHomeDir()
	verifier := &identity.Verifier{Issuer: a.Cfg.Issuer, Keys: &identity.RemoteKeys{URL: a.Cfg.JWKSURL}}
	srv, err := sshd.New(sshd.Config{
		HostKey: signer,
		Authorizer: &sshd.Authorizer{
			Verifier:   verifier,
			ComputerID: dev.ComputerID,
			Own:        dev.Own,
			Ten:        dev.Ten,
			MaxAge:     time.Duration(a.Cfg.TokenMaxAgeSeconds) * time.Second,
		},
		Shell:   sh,
		BaseEnv: env,
		Home:    home,
		Version: a.Version,
		Log:     a.Log.With("part", "sshd"),
		Activity: func(act sshd.Activity) {
			say(a.Console, ActivityLine(act))
		},
	})
	if err != nil {
		return err
	}
	defer srv.Close()

	ui.Status(tray.Connecting, "正在连接")
	var nw *network.Network
	for backoff := 5 * time.Second; ; backoff = min(backoff*2, 5*time.Minute) {
		opts, err := a.joinOptions(ctx, dev, join)
		if err == nil {
			startCtx, cancel := context.WithTimeout(ctx, 2*time.Minute)
			nw, err = network.Start(startCtx, opts)
			cancel()
		}
		if err == nil {
			break
		}
		if errors.Is(err, identity.ErrDeviceRemoved) {
			return err
		}
		// A peer that cannot join may have been removed: ask the issuer, which knows for sure.
		if !a.Headless {
			if _, terr := a.token(ctx, identity.AudienceCompute); errors.Is(terr, identity.ErrDeviceRemoved) {
				return terr
			}
		}
		a.Log.Warn("network not up; retrying", "in", backoff.String(), "error", err.Error())
		ui.Status(tray.Offline, "连不上网络，稍后重试")
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(backoff):
		}
	}
	defer nw.Stop()
	if !dev.Joined {
		dev.Joined = true
		if err := a.Dir.SaveDevice(dev); err != nil {
			a.Log.Warn("could not record the first join", "error", err.Error())
		}
	}
	st := nw.Status()
	a.Log.Info("joined", "ip", st.IP, "fqdn", st.FQDN, "management", st.Connected)

	var wg sync.WaitGroup
	wg.Go(func() { a.serveSSH(ctx, nw, srv) })
	if !a.Headless {
		wg.Go(func() { a.reportFacts(ctx, stop, dev) })
	}
	wg.Go(func() { a.watch(ctx, stop, nw, dev, ui) })
	if !a.Headless {
		wg.Go(func() { a.watchUpdates(ctx) })
	}
	<-ctx.Done()
	_ = srv.Close()
	wg.Wait()
	return nil
}

// watchUpdates tells the person, in the window and the log, when a newer release exists (release/README.md
// "Updates": tell, never replace). A minute after the device is online, then daily.
func (a *App) watchUpdates(ctx context.Context) {
	update.Watch(ctx, a.Cfg.UpdateURL, a.Version, time.Minute, 24*time.Hour, func(r *update.Release) {
		a.Log.Info("update available", "version", r.Version, "release", r.Page, "download", a.Cfg.DownloadURL)
		say(a.Console, time.Now().Format("15:04:05")+"  "+update.Notice(r, a.Cfg.DownloadURL))
	}, func(err error) {
		a.Log.Info("update check failed (the device is unaffected)", "error", err.Error())
	})
}

// serveSSH listens on the NetBird address and listens again when the listener ends (the engine restarted).
func (a *App) serveSSH(ctx context.Context, nw *network.Network, srv *sshd.Server) {
	for backoff := time.Second; ctx.Err() == nil; backoff = min(backoff*2, time.Minute) {
		ln, err := nw.Listen(a.Cfg.SSHPort)
		if err != nil {
			a.Log.Warn("ssh: listen failed; retrying", "in", backoff.String(), "error", err.Error())
		} else {
			backoff = time.Second
			a.Log.Info("ssh listening", "addr", ln.Addr().String())
			stop := context.AfterFunc(ctx, func() { _ = ln.Close() })
			err = srv.Serve(ln)
			stop()
			if ctx.Err() != nil {
				return
			}
			a.Log.Warn("ssh listener ended; listening again", "error", errString(err))
		}
		select {
		case <-ctx.Done():
			return
		case <-time.After(backoff):
		}
	}
}

func errString(err error) string {
	if err == nil {
		return ""
	}
	if errors.Is(err, net.ErrClosed) {
		return "listener closed"
	}
	return err.Error()
}

// reportFacts reports the facts at start and again whenever they change (checked hourly).
func (a *App) reportFacts(ctx context.Context, stop context.CancelCauseFunc, dev *state.Device) {
	var last []byte
	tick := time.NewTicker(time.Hour)
	defer tick.Stop()
	for {
		f := facts.Collect(ctx)
		cmp := f
		cmp.ObservedAt = time.Time{}
		raw, _ := json.Marshal(cmp)
		if string(raw) != string(last) {
			err := a.putFacts(ctx, dev, f)
			if err == nil {
				last = raw
			} else if errors.Is(err, identity.ErrDeviceRemoved) {
				stop(err)
				return
			} else {
				a.Log.Warn("facts not reported; retrying within the hour", "error", err.Error())
			}
		}
		select {
		case <-ctx.Done():
			return
		case <-tick.C:
		}
	}
}

// putFacts reports the facts. compute/v1 answering that this computer does not exist means it was removed: that is
// identity.ErrDeviceRemoved, the one way the device ends (as a refused login does).
func (a *App) putFacts(ctx context.Context, dev *state.Device, f facts.Facts) error {
	tok, err := a.token(ctx, identity.AudienceCompute)
	if err != nil {
		return err
	}
	err = a.computeClient().ReportFacts(ctx, tok, dev.ComputerID, f)
	if compute.IsComputerGone(err) {
		return errors.Join(identity.ErrDeviceRemoved, fmt.Errorf("compute/v1 no longer has %s (%s): it was removed from the account: %w", dev.ComputerID, dev.Name, err))
	}
	return err
}

// stillThereEvery is how often a device whose network is down asks compute/v1 whether it still exists.
const stillThereEvery = time.Minute

// watch shows the network state in the tray. When the network drops it asks compute/v1 at once, then every minute
// while it stays down, whether this computer still exists: removing a device (我的设备, DELETE /v1/devices) deletes
// its NetBird peer, which is what drops the network, while its login may go on working (AgentDevice#231).
func (a *App) watch(ctx context.Context, stop context.CancelCauseFunc, nw *network.Network, dev *state.Device, ui UI) {
	last := ""
	var asked time.Time
	for {
		st := nw.Status()
		text, s := "在线", tray.Online
		if !st.Connected {
			text, s = "连接断了，正在重连", tray.Offline
		}
		if text != last {
			ui.Status(s, text)
			a.Log.Info("state", "connected", st.Connected, "ip", st.IP, "error", st.Error)
			last = text
			asked = time.Time{}
		}
		if !st.Connected && !a.Headless && time.Since(asked) >= stillThereEvery {
			asked = time.Now()
			if err := a.putFacts(ctx, dev, facts.Collect(ctx)); errors.Is(err, identity.ErrDeviceRemoved) {
				stop(err)
				return
			} else if err != nil {
				a.Log.Warn("network down and compute/v1 not reachable either; retrying", "in", stillThereEvery.String(), "error", err.Error())
			} else {
				a.Log.Info("network down; compute/v1 still has this computer, reconnecting", "computer", dev.ComputerID)
			}
		}
		select {
		case <-ctx.Done():
			return
		case <-time.After(10 * time.Second):
		}
	}
}

// JoinOnce joins the network with the one-use credential and leaves again, so the peer key is saved for later
// starts (`agentdevice login` on a computer without a desktop, followed by `agentdevice autostart on`).
func (a *App) JoinOnce(ctx context.Context, join *compute.Join) error {
	dev, err := a.Dir.LoadDevice()
	if err != nil || dev == nil {
		return fmt.Errorf("join: no enrolment (%v)", err)
	}
	opts, err := a.joinOptions(ctx, dev, join)
	if err != nil {
		return err
	}
	startCtx, cancel := context.WithTimeout(ctx, 2*time.Minute)
	defer cancel()
	nw, err := network.Start(startCtx, opts)
	if err != nil {
		return err
	}
	dev.Joined = true
	saveErr := a.Dir.SaveDevice(dev)
	_ = nw.Stop()
	return saveErr
}
