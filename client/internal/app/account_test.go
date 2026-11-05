package app

import (
	"errors"
	"os/user"
	"testing"
)

// AgentDevice#229: an account without a passwd entry (a container's bare uid) must still enrol with a user name.
func TestAccountName(t *testing.T) {
	none := func() (*user.User, error) { return nil, errors.New("user: unknown userid 1000") }
	env := func(m map[string]string) func(string) string { return func(k string) string { return m[k] } }
	uid := func(n int) func() int { return func() int { return n } }
	cases := []struct {
		name    string
		current func() (*user.User, error)
		env     map[string]string
		uid     int
		want    string
	}{
		{"passwd entry", func() (*user.User, error) { return &user.User{Username: "dev"}, nil }, nil, 1000, "dev"},
		{"windows domain prefix", func() (*user.User, error) { return &user.User{Username: `PC\maxy`}, nil }, nil, -1, "maxy"},
		{"no entry, USER", none, map[string]string{"USER": "node"}, 1000, "node"},
		{"no entry, LOGNAME", none, map[string]string{"LOGNAME": "work"}, 1000, "work"},
		{"no entry, no env: the uid", none, nil, 1000, "1000"},
		{"nothing at all", none, nil, -1, ""},
	}
	for _, c := range cases {
		if got := accountName(c.current, env(c.env), uid(c.uid)); got != c.want {
			t.Errorf("%s: accountName = %q, want %q", c.name, got, c.want)
		}
	}
}
