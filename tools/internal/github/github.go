// Package github calls GitHub's REST API with the standard library: the
// runner service's machines needn't have gh. The token comes from
// GITHUB_TOKEN or GH_TOKEN, and the repository from GITHUB_REPOSITORY.
// Neither is ever printed.
package github

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"regexp"
	"strings"
	"time"
)

// API is GitHub's REST API.
const API = "https://api.github.com"

// Error is an answer GitHub gave other than success, by its status.
type Error struct {
	Method, Path string
	Status       int
}

func (e *Error) Error() string {
	return fmt.Sprintf("GitHub answered %s %s with %d", e.Method, e.Path, e.Status)
}

// Client calls the API. The zero value uses the environment and
// http.DefaultClient.
type Client struct {
	// Getenv is os.Getenv unless set.
	Getenv func(string) string
	// HTTP is http.DefaultClient unless set.
	HTTP *http.Client
	// Base is API unless set.
	Base string
}

func (c *Client) getenv(name string) string {
	if c.Getenv != nil {
		return c.Getenv(name)
	}
	return os.Getenv(name)
}

var repoName = regexp.MustCompile(`^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$`)

// Repository is GITHUB_REPOSITORY, owner/name.
func (c *Client) Repository() (string, error) {
	name := c.getenv("GITHUB_REPOSITORY")
	if !repoName.MatchString(name) {
		return "", errors.New("GITHUB_REPOSITORY must name the repository, owner/name")
	}
	return name, nil
}

func (c *Client) token() (string, error) {
	if t := c.getenv("GITHUB_TOKEN"); t != "" {
		return t, nil
	}
	if t := c.getenv("GH_TOKEN"); t != "" {
		return t, nil
	}
	return "", errors.New("GITHUB_TOKEN must be set to call GitHub")
}

var next = regexp.MustCompile(`<([^>]+)>;\s*rel="next"`)

// Request calls path, such as repos/o/r/git/refs, or a full API URL, with
// body as JSON if not nil, and decodes the answer into out if not nil. It
// returns the next page's URL, if any.
func (c *Client) Request(method, path string, body, out any) (string, error) {
	base := c.Base
	if base == "" {
		base = API
	}
	url := path
	if !strings.HasPrefix(path, base+"/") {
		url = base + "/" + path
	}
	token, err := c.token()
	if err != nil {
		return "", err
	}
	var reader io.Reader
	if body != nil {
		data, err := json.Marshal(body)
		if err != nil {
			return "", err
		}
		reader = bytes.NewReader(data)
	}
	req, err := http.NewRequest(method, url, reader)
	if err != nil {
		return "", err
	}
	req.Header.Set("Accept", "application/vnd.github+json")
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("X-GitHub-Api-Version", "2022-11-28")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	client := c.HTTP
	if client == nil {
		client = &http.Client{Timeout: 60 * time.Second}
	}
	resp, err := client.Do(req)
	if err != nil {
		// The error names the URL, never the token.
		return "", fmt.Errorf("calling GitHub: %s %s: %w", method, strings.TrimPrefix(url, base+"/"), unwrapURL(err))
	}
	defer resp.Body.Close()
	payload, err := io.ReadAll(io.LimitReader(resp.Body, 64<<20))
	if err != nil {
		return "", err
	}
	if resp.StatusCode < 200 || resp.StatusCode > 299 {
		return "", &Error{Method: method, Path: strings.TrimPrefix(url, base+"/"), Status: resp.StatusCode}
	}
	if out != nil && len(payload) > 0 {
		if err := json.Unmarshal(payload, out); err != nil {
			return "", fmt.Errorf("GitHub's answer to %s %s: %w", method, strings.TrimPrefix(url, base+"/"), err)
		}
	}
	if m := next.FindStringSubmatch(resp.Header.Get("Link")); m != nil {
		return m[1], nil
	}
	return "", nil
}

func unwrapURL(err error) error {
	var u interface{ Unwrap() error }
	if errors.As(err, &u) {
		if inner := u.Unwrap(); inner != nil {
			return inner
		}
	}
	return err
}

// Paginate is every item of a list endpoint, following its pages.
func Paginate[T any](c *Client, path string) ([]T, error) {
	sep := "?"
	if strings.Contains(path, "?") {
		sep = "&"
	}
	var items []T
	url := path + sep + "per_page=100"
	for url != "" {
		var page []T
		var err error
		url, err = c.Request(http.MethodGet, url, nil, &page)
		if err != nil {
			return nil, err
		}
		items = append(items, page...)
	}
	return items, nil
}
