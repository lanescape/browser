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

// A refusal is one even if its body is cut short: a 422's status reaches
// the caller, which looks for the colliding tag.
func TestARefusalCutShortKeepsItsStatus(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Length", "100")
		w.WriteHeader(422)
		w.Write([]byte(`{"mess`))
		w.(http.Flusher).Flush()
		panic(http.ErrAbortHandler)
	}))
	defer srv.Close()
	c := &Client{Getenv: env(map[string]string{"GITHUB_TOKEN": "TOKEN"}), Base: srv.URL}
	_, err := c.Request(http.MethodPost, "repos/lanescape/browser/git/refs", map[string]string{"ref": "refs/tags/x"}, nil)
	var e *Error
	if !errors.As(err, &e) || e.Status != 422 {
		t.Fatalf("err = %v", err)
	}
}

// A page that isn't a list, or an answer that's missing, is an error, not
// an empty listing.
func TestAMissingPageIsAnError(t *testing.T) {
	for _, body := range []string{"", "null", "{}", " "} {
		srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			w.Write([]byte(body))
		}))
		c := &Client{Getenv: env(map[string]string{"GITHUB_TOKEN": "TOKEN"}), Base: srv.URL}
		if got, err := Paginate[int](c, "repos/lanescape/browser/git/matching-refs/tags/"); err == nil {
			t.Errorf("%q: %v", body, got)
		}
		srv.Close()
	}
}

// A listing whose next page is one already read is an error, not a loop.
func TestARepeatedPageIsAnError(t *testing.T) {
	var srv *httptest.Server
	srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Link", `<`+srv.URL+`/x?page=2>; rel="next"`)
		w.Write([]byte(`[1]`))
	}))
	defer srv.Close()
	c := &Client{Getenv: env(map[string]string{"GITHUB_TOKEN": "TOKEN"}), Base: srv.URL}
	if _, err := Paginate[int](c, "x"); err == nil || !strings.Contains(err.Error(), "repeats") {
		t.Fatalf("err = %v", err)
	}
}

// A next page in a second Link header is followed; one outside the API is
// an error; and a failed page's error holds no query.
func TestEveryLinkHeaderCounts(t *testing.T) {
	var srv *httptest.Server
	srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Query().Get("page") {
		case "":
			w.Header().Add("Link", `<`+srv.URL+`/x?page=9>; rel="last"`)
			w.Header().Add("Link", `<`+srv.URL+`/x?page=2>; rel="next"`)
			w.Write([]byte(`[1]`))
		case "2":
			w.Write([]byte(`[2]`))
		}
	}))
	defer srv.Close()
	c := &Client{Getenv: env(map[string]string{"GITHUB_TOKEN": "TOKEN"}), Base: srv.URL}
	if got, err := Paginate[int](c, "x"); err != nil || len(got) != 2 {
		t.Fatalf("Paginate = %v, %v", got, err)
	}

	away := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Link", `<https://elsewhere.example/x?page=2>; rel="next"`)
		w.Write([]byte(`[1]`))
	}))
	defer away.Close()
	c = &Client{Getenv: env(map[string]string{"GITHUB_TOKEN": "TOKEN"}), Base: away.URL}
	if _, err := Paginate[int](c, "x"); err == nil || strings.Contains(err.Error(), "elsewhere") {
		t.Fatalf("err = %v", err)
	}

	failing := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("page") == "2" {
			w.WriteHeader(404)
			return
		}
		w.Header().Set("Link", `<`+"http://"+r.Host+`/x?page=2&sig=deadbeef>; rel="next"`)
		w.Write([]byte(`[1]`))
	}))
	defer failing.Close()
	c = &Client{Getenv: env(map[string]string{"GITHUB_TOKEN": "TOKEN"}), Base: failing.URL}
	_, err := Paginate[int](c, "x")
	var e *Error
	if !errors.As(err, &e) || e.Status != 404 || strings.Contains(err.Error(), "deadbeef") {
		t.Fatalf("err = %v", err)
	}
}
