package github

import (
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func env(m map[string]string) func(string) string { return func(k string) string { return m[k] } }

// GitHub's API without gh: requests carry the token, pages are followed,
// and refusals keep their status and never the token.
func TestPagesAreFollowed(t *testing.T) {
	var srv *httptest.Server
	var auth []string
	srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		auth = append(auth, r.Header.Get("Authorization"))
		if r.URL.Query().Get("page") == "2" {
			w.Write([]byte(`[3]`))
			return
		}
		if r.URL.Path != "/repos/lanescape/browser/git/matching-refs/tags/" || r.URL.Query().Get("per_page") != "100" {
			t.Errorf("first page at %s", r.URL)
		}
		w.Header().Set("Link", `<`+srv.URL+`/x?page=2>; rel="next", <`+srv.URL+`/x?page=2>; rel="last"`)
		w.Write([]byte(`[1, 2]`))
	}))
	defer srv.Close()
	c := &Client{Getenv: env(map[string]string{"GITHUB_TOKEN": "TOKEN"}), Base: srv.URL}
	got, err := Paginate[int](c, "repos/lanescape/browser/git/matching-refs/tags/")
	if err != nil || len(got) != 3 || got[2] != 3 {
		t.Fatalf("Paginate = %v, %v", got, err)
	}
	if len(auth) != 2 || auth[0] != "Bearer TOKEN" || auth[1] != "Bearer TOKEN" {
		t.Errorf("Authorization %v", auth)
	}
}

func TestARefusalKeepsItsStatusAndNotTheToken(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(422)
		w.Write([]byte(`{}`))
	}))
	defer srv.Close()
	c := &Client{Getenv: env(map[string]string{"GITHUB_TOKEN": "TOKEN"}), Base: srv.URL}
	_, err := c.Request(http.MethodPost, "repos/lanescape/browser/git/refs", map[string]string{"ref": "refs/tags/x"}, nil)
	var e *Error
	if !errors.As(err, &e) || e.Status != 422 {
		t.Fatalf("err = %v", err)
	}
	if strings.Contains(err.Error(), "TOKEN") {
		t.Fatal("the error holds the token")
	}
}

func TestTheTokenAndRepositoryAreRequired(t *testing.T) {
	if _, err := (&Client{Getenv: env(nil)}).Request(http.MethodGet, "x", nil, nil); err == nil || !strings.Contains(err.Error(), "GITHUB_TOKEN") {
		t.Errorf("no token: %v", err)
	}
	if _, err := (&Client{Getenv: env(map[string]string{"GITHUB_REPOSITORY": "nope"})}).Repository(); err == nil {
		t.Error("accepted a repository without an owner")
	}
	if r, err := (&Client{Getenv: env(map[string]string{"GITHUB_REPOSITORY": "lanescape/browser"})}).Repository(); err != nil || r != "lanescape/browser" {
		t.Errorf("Repository = %q, %v", r, err)
	}
	// GH_TOKEN serves too.
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer GH" {
			w.WriteHeader(401)
		}
	}))
	defer srv.Close()
	if _, err := (&Client{Getenv: env(map[string]string{"GH_TOKEN": "GH"}), Base: srv.URL}).Request(http.MethodGet, "x", nil, nil); err != nil {
		t.Errorf("GH_TOKEN: %v", err)
	}
}
