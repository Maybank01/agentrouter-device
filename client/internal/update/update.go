// Package update tells the person when a newer release exists (release/README.md "Updates"). It only checks and
// tells: it downloads and replaces nothing. Self-replacement waits for a release signing key the client can pin
// (docs/development/AV-HYGIENE.md "自动更新"; code signing is the owner's decision, docs/adr/0001-code-signing.md); until
// then the person runs the environment's installer, which replaces the program in place and keeps the login.
package update

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"time"
)

// TagPrefix is the release tags' prefix (`client-v1.2.3`); pre-releases are `client-v0.0.0-<sha7>`.
const TagPrefix = "client-v"

// Release is the newest release as the update URL answers (GitHub's `GET /repos/{owner}/{repo}/releases/latest`).
type Release struct {
	Tag     string `json:"tag_name"`
	Page    string `json:"html_url"`
	Version string `json:"-"`
}

// Version is a release's X.Y.Z; ok is false for anything else (a developer build "dev", a pre-release
// "0.0.0-<sha7>").
func Version(s string) (v [3]int, ok bool) {
	parts := strings.Split(strings.TrimPrefix(strings.TrimPrefix(s, TagPrefix), "v"), ".")
	if len(parts) != 3 {
		return v, false
	}
	for i, p := range parts {
		n, err := strconv.Atoi(p)
		if err != nil || n < 0 || p != strconv.Itoa(n) {
			return v, false
		}
		v[i] = n
	}
	return v, true
}

// Newer says whether release version b is newer than a.
func Newer(a, b [3]int) bool {
	for i := range a {
		if a[i] != b[i] {
			return b[i] > a[i]
		}
	}
	return false
}

// Check asks url for the newest release; nil when it is not newer than current or current is not a release.
func Check(ctx context.Context, client *http.Client, url, current string) (*Release, error) {
	cur, ok := Version(current)
	if !ok || url == "" {
		return nil, nil
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, fmt.Errorf("update: GET %s: %w", url, err)
	}
	req.Header.Set("Accept", "application/vnd.github+json")
	req.Header.Set("User-Agent", "agentdevice/"+current)
	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("update: GET %s: %w (a retry later may help)", url, err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("update: GET %s answered %d: %.200s", url, resp.StatusCode, strings.TrimSpace(string(body)))
	}
	var r Release
	if err := json.Unmarshal(body, &r); err != nil {
		return nil, fmt.Errorf("update: GET %s: not a release (JSON with tag_name): %w", url, err)
	}
	v, ok := Version(r.Tag)
	if !ok || !strings.HasPrefix(r.Tag, TagPrefix) {
		return nil, fmt.Errorf("update: GET %s: tag %q is not %sX.Y.Z", url, r.Tag, TagPrefix)
	}
	if !Newer(cur, v) {
		return nil, nil
	}
	r.Version = fmt.Sprintf("%d.%d.%d", v[0], v[1], v[2])
	return &r, nil
}

// Notice is the line the person sees (window and log): the version and where to get it.
func Notice(r *Release, download string) string {
	where := download
	if where == "" {
		where = r.Page
	}
	return "有新版本 " + r.Version + "，下载安装即可替换旧版：" + where
}

// Watch checks after first, then every interval, and calls tell once per newer version it finds. Errors go to
// logf only: an unreachable update URL never disturbs the device.
func Watch(ctx context.Context, url, current string, first, interval time.Duration, tell func(*Release), logf func(error)) {
	if _, ok := Version(current); !ok || url == "" {
		return
	}
	client := &http.Client{}
	told := ""
	wait := first
	for {
		select {
		case <-ctx.Done():
			return
		case <-time.After(wait):
		}
		wait = interval
		r, err := Check(ctx, client, url, current)
		if err != nil {
			if ctx.Err() == nil {
				logf(err)
			}
			continue
		}
		if r != nil && r.Version != told {
			told = r.Version
			tell(r)
		}
	}
}
