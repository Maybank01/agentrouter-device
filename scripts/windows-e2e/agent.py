#!/usr/bin/env python3
"""The Agent side of the Windows device end-to-end check on Dev (#231; workflow windows-e2e.yml runs the device).

Runs where an operator can reach Dev: the workstation, through an SSH tunnel to the in-network SSH gateway (compute/v1
has no public SSH name yet, AgentCompute docs/operations/compute-dev.md). Every step is the one the product takes:

  1. 我的设备: GET /v1/computers as ECS lists them for the person (STS `ecs` key, act-for, computer:*:read) until the
     runner's device is online.
  2. AgentHost's way in: POST /v1/computers/{id}/access (STS `agenthost` key, act-for, computer:{id}:ssh), a run
     token aud netbird for the SSH password (askpass from a file, never argv), OpenSSH through the gateway with both
     host keys pinned (the same ssh config AgentHost's dsh-runtime writes).
  3. On the device: whoami, whoami /groups (must not be an administrator), an SFTP write and read-back.
  4. Revoke: DELETE /v1/devices/{id} (the owner's removal, computer:{id}:manage) while a session is open. The open
     session must end at once, a new access must be refused, a new ssh must be refused, the list must not show it.

Usage (stdlib only):
  python3 scripts/windows-e2e/agent.py --name gh-win-<run id>-<attempt> [--keys DIR] [--own acc_…] [--out DIR]
Service keys are read from --keys (agenthost.key, ecs.key; default the workstation's .secrets copy); no token or key
is printed or put on a command line.
"""
import argparse, json, os, shlex, signal, socket, subprocess, sys, tempfile, time, urllib.error, urllib.request

STS = "https://identity-dev.agentrouter.page"
COMPUTE = "https://compute-dev.agentrouter.page"
OWN = "acc_398219269629808740"  # Dev ZITADEL test person device-e2e (product organisation)
KEYS = os.path.expanduser("~/AgentTeam/.secrets/agentidentity-dev/service-keys")
HOST = "agentrouter-prod"  # the shared Dev host (operators reach the in-network gateway through it)
GATEWAY_CONTAINER = "agentcompute-ssh-gateway-dev"

T0 = time.time()
evidence = []


def log(what, **kv):
    line = {"t": round(time.time() - T0, 2), "step": what, **kv}
    evidence.append(line)
    print(json.dumps(line, ensure_ascii=False), flush=True)


def fail(what, **kv):
    log("FAIL " + what, **kv)
    raise SystemExit(1)


def http(method, url, token, body=None):
    data = json.dumps(body).encode() if body is not None else None
    h = {"Authorization": "Bearer " + token, "Accept": "application/json, application/problem+json"}
    if data is not None:
        h["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, method=method, headers=h)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            raw = r.read().decode()
            return r.status, (json.loads(raw) if raw.strip() else None)
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, raw[:500]


class Tokens:
    def __init__(self, keys, own):
        self.key = {n: open(os.path.join(keys, f"{n}.key")).read().strip() for n in ("agenthost", "ecs")}
        self.own = own

    def mint(self, service, aud, scp, run=None):
        body = {"aud": aud, "scp": scp, "own": self.own, "pay": self.own}
        if run:
            body["run"] = run
        st, res = http("POST", STS + "/v1/identity/token", self.key[service], body)
        if st != 200:
            fail("STS token", service=service, aud=aud, scp=scp, status=st, answer=res)
        return res["access_token"]


def list_computers(tok):
    st, res = http("GET", COMPUTE + "/v1/computers", tok.mint("ecs", "agentcompute", ["computer:*:read"]))
    if st != 200:
        fail("GET /v1/computers", status=st, answer=res)
    return res["computers"]


class Tunnel:
    """ssh -L to the gateway's in-network address on the Dev host; stopped by PID."""

    def __init__(self):
        out = subprocess.run(["ssh", HOST, "docker", "inspect", GATEWAY_CONTAINER, "--format",
                              '{{(index .NetworkSettings.Networks "coolify").IPAddress}}'], capture_output=True, text=True, timeout=60)
        ip = out.stdout.strip()
        if out.returncode != 0 or not ip:
            fail("gateway address on the Dev host", stderr=out.stderr.strip()[:300])
        s = socket.socket(); s.bind(("127.0.0.1", 0)); self.port = s.getsockname()[1]; s.close()
        self.proc = subprocess.Popen(["ssh", "-N", "-o", "ExitOnForwardFailure=yes", "-L", f"127.0.0.1:{self.port}:{ip}:2222", HOST])
        for _ in range(60):
            try:
                socket.create_connection(("127.0.0.1", self.port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.5)
        else:
            fail("tunnel to the gateway")
        log("tunnel to the SSH gateway", local=f"127.0.0.1:{self.port}", gateway=f"{GATEWAY_CONTAINER} ({ip}):2222")

    def stop(self):
        self.proc.send_signal(signal.SIGTERM)
        self.proc.wait(timeout=10)


class SSH:
    """AgentHost's ssh config (dsh-runtime env/sandbox.mjs sshConfig) with the jump at the tunnel."""

    def __init__(self, work, tunnel, cmp, access):
        self.work, self.cmp, self.access = work, cmp, access
        target_host, target_port = access["target"].rsplit(":", 1)
        alias, jalias = f"e2e-{cmp}", f"e2e-{cmp}-jump"
        key = lambda k: " ".join(k.strip().split()[:2])
        with open(os.path.join(work, "known_hosts"), "w") as f:
            f.write(f"{alias} {key(access['hostKey'])}\n{jalias} {key(access.get('jumpHostKey') or access['hostKey'])}\n")
        self.token_file = os.path.join(work, "token")
        askpass = os.path.join(work, "askpass")
        with open(askpass, "w") as f:
            f.write(f"#!/bin/sh\nexec cat {shlex.quote(self.token_file)}\n")
        os.chmod(askpass, 0o700)
        kh = os.path.join(work, "known_hosts")
        common = f"-o UserKnownHostsFile={kh} -o StrictHostKeyChecking=yes -o PreferredAuthentications=password -o NumberOfPasswordPrompts=1"
        self.config = os.path.join(work, "ssh_config")
        with open(self.config, "w") as f:
            f.write("\n".join([
                "Host win", f"  HostName {target_host}", f"  Port {target_port}", f"  User {access['user']}",
                f"  HostKeyAlias {alias}", f"  UserKnownHostsFile {kh}", "  StrictHostKeyChecking yes",
                "  PreferredAuthentications password", "  NumberOfPasswordPrompts 1", "  LogLevel ERROR",
                f"  ProxyCommand ssh -F none {common} -o HostKeyAlias={jalias} -p {tunnel.port} -W %h:%p {access['user']}@127.0.0.1", ""]))
        self.env = {**os.environ, "SSH_ASKPASS": askpass, "SSH_ASKPASS_REQUIRE": "force", "DISPLAY": ":0"}

    def credential(self, tok, run):
        # A run token for this computer (NetBird's SSH server and the gateway refuse one older than 600 s).
        fd = os.open(self.token_file, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            f.write(tok.mint("agenthost", "netbird", [f"computer:{self.cmp}:ssh"], run=run))

    def run(self, command, timeout=90):
        p = subprocess.run(["ssh", "-F", self.config, "win", command], capture_output=True, timeout=timeout, env=self.env)
        return p.returncode, p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace")

    def popen(self, command):
        return subprocess.Popen(["ssh", "-F", self.config, "win", command], stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=self.env)

    def sftp(self, batch):
        b = os.path.join(self.work, "sftp.batch")
        with open(b, "w") as f:
            f.write(batch)
        p = subprocess.run(["sftp", "-F", self.config, "-b", b, "win"], capture_output=True, timeout=120, env=self.env)
        return p.returncode, p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--name", required=True, help="the device name the workflow enrolled (gh-win-<run id>-<attempt>)")
    ap.add_argument("--own", default=OWN)
    ap.add_argument("--keys", default=KEYS)
    ap.add_argument("--wait", type=int, default=600, help="seconds to wait for the device to be online")
    ap.add_argument("--out", default=None, help="write the evidence lines here (JSON)")
    a = ap.parse_args()
    tok = Tokens(a.keys, a.own)
    run = "cnv_wine2e" + a.name.replace("gh-win-", "").replace("-", "_")
    work = tempfile.mkdtemp(prefix="win-e2e-")
    os.chmod(work, 0o700)
    tunnel = None
    try:
        # 1. 我的设备
        deadline = time.time() + a.wait
        while True:
            mine = [c for c in list_computers(tok) if c.get("name") == a.name]
            if mine and mine[0].get("state") == "online":
                break
            if time.time() > deadline:
                fail("device online in 我的设备", seen=mine)
            time.sleep(5)
        c = mine[0]
        cmp = c["id"]
        f = c.get("facts") or {}
        log("我的设备 lists the device (GET /v1/computers)", id=cmp, name=c["name"], kind=c.get("kind"), state=c["state"],
            os=f.get("os"), arch=f.get("arch"), shell=f.get("shell"), clientVersion=c.get("clientVersion"))

        # 2. access, as AgentHost asks for it
        st, acc = http("POST", f"{COMPUTE}/v1/computers/{cmp}/access", tok.mint("agenthost", "agentcompute", [f"computer:{cmp}:ssh"]), {})
        if st != 200:
            fail("POST /access", status=st, answer=acc)
        log("POST /v1/computers/{id}/access", jump=acc["jump"], target=acc["target"], user=acc["user"], tokenMaxAgeSeconds=acc.get("tokenMaxAgeSeconds"))
        tunnel = Tunnel()
        ssh = SSH(work, tunnel, cmp, acc)

        # 3. on the device, as its account
        ssh.credential(tok, run)
        t = time.time(); rc, out, err = ssh.run("whoami")
        if rc != 0:
            fail("ssh whoami", rc=rc, stderr=err.strip()[-500:])
        log("ssh whoami", rc=rc, out=out.strip(), seconds=round(time.time() - t, 1))
        rc, groups, err = ssh.run("whoami /groups")
        if rc != 0:
            fail("ssh whoami /groups", rc=rc, stderr=err.strip()[-500:])
        admin = "S-1-5-32-544" in groups or "BUILTIN\\Administrators" in groups
        lines = [" ".join(l.split()[:1] + l.split()[2:3]) for l in groups.splitlines() if l.strip().startswith(("BUILTIN", "NT AUTHORITY", "Mandatory", "Everyone", "LOCAL", "CONSOLE"))]
        log("ssh whoami /groups", administrator=admin, groups=lines)
        if admin:
            fail("the SSH session runs as an administrator", groups=groups)
        rc, priv, _ = ssh.run("whoami /priv")
        log("ssh whoami /priv", privileges=[l.split()[0] for l in priv.splitlines() if l.startswith("Se")])

        body = f"AgentDevice #231 e2e {a.name} {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}\n中文：设备端只做执行\n"
        local = os.path.join(work, "up.txt"); back = os.path.join(work, "down.txt")
        with open(local, "w", encoding="utf-8") as fh:
            fh.write(body)
        remote = f"e2e-{a.name}.txt"
        rc, out, err = ssh.sftp(f"put {local} {remote}\nls -l {remote}\nget {remote} {back}\n")
        if rc != 0:
            fail("sftp put/get", rc=rc, stdout=out[-500:], stderr=err.strip()[-500:])
        same = open(back, encoding="utf-8").read() == body
        log("sftp put + ls + get", same_bytes=same, listing=[l for l in out.splitlines() if remote in l and not l.startswith("sftp>")])
        if not same:
            fail("sftp read-back differs")
        rc, out, err = ssh.run(f"type {remote}")
        log("ssh type <file> (the shell reads what SFTP wrote)", rc=rc, first_line=out.splitlines()[0] if out else "", stderr=err.strip()[-200:])

        # 4. remove the device while a session is open
        ssh.credential(tok, run)
        long = ssh.popen("ping -n 600 127.0.0.1")
        first = long.stdout.readline().decode("utf-8", "replace").strip()
        while first == "":
            first = long.stdout.readline().decode("utf-8", "replace").strip()
            if long.poll() is not None:
                fail("long session did not start", stderr=long.stderr.read().decode()[-300:])
        time.sleep(3)
        log("open session (ping -n 600 on the device)", first_line=first, running=long.poll() is None)
        st, res = http("DELETE", f"{COMPUTE}/v1/devices/{cmp}", tok.mint("agenthost", "agentcompute", [f"computer:{cmp}:manage"]))
        t_removed = time.time()
        log("DELETE /v1/devices/{id}", status=st, answer=res)
        if st != 204:
            fail("DELETE /v1/devices", status=st, answer=res)
        try:
            long.wait(timeout=60)
            ended = round(time.time() - t_removed, 2)
            log("the open session ended", seconds_after_removal=ended, rc=long.returncode, stderr=long.stderr.read().decode("utf-8", "replace").strip()[-300:])
            session_cut = ended <= 5
        except subprocess.TimeoutExpired:
            log("the open session was STILL OPEN 60 s after the removal")
            long.kill()
            long.wait()
            session_cut = False

        st, res = http("POST", f"{COMPUTE}/v1/computers/{cmp}/access", tok.mint("agenthost", "agentcompute", [f"computer:{cmp}:ssh"]), {})
        log("POST /access after the removal", status=st, code=(res or {}).get("code") if isinstance(res, dict) else res)
        ssh.credential(tok, run + "_after")
        t = time.time(); rc, out, err = ssh.run("whoami", timeout=60)
        log("new ssh after the removal", rc=rc, seconds=round(time.time() - t, 1), stderr=err.strip()[-300:])
        listed = [x for x in list_computers(tok) if x.get("id") == cmp]
        log("我的设备 after the removal", listed=bool(listed))
        ok = session_cut and st == 404 and rc != 0 and not listed
        log("RESULT", passed=ok, session_cut_at_once=session_cut, access_refused=st == 404, new_ssh_refused=rc != 0, gone_from_list=not listed)
        if not ok:
            raise SystemExit(1)
    finally:
        if tunnel:
            tunnel.stop()
        if a.out:
            with open(a.out, "w") as fh:
                json.dump(evidence, fh, ensure_ascii=False, indent=1)
        subprocess.run(["rm", "-rf", work])


if __name__ == "__main__":
    main()
