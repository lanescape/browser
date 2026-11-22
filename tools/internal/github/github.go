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

	"github.com/lanescape/browser/tools/internal/fetch"
)

// maxAnswer is the most of an answer read.
const maxAnswer = 64 << 20

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

// nextLink is the target of the Link headers' rel="next" link, parsed as
// RFC 8288 has them: links separated by commas, each a <target> followed by
// parameters; rel's value is a list of relation types. Two different next
// links, or a link that doesn't parse, is an error rather than a guess.
func nextLink(headers []string) (string, error) {
	found := ""
	for _, header := range headers {
		rest := strings.TrimSpace(header)
		for rest != "" {
			if rest[0] != '<' {
				return "", errors.New("a Link header doesn't parse")
			}
			end := strings.IndexByte(rest, '>')
			if end < 0 {
				return "", errors.New("a Link header doesn't parse")
			}
			target := rest[1:end]
			rest = rest[end+1:]
			isNext := false
			for {
				rest = strings.TrimLeft(rest, " \t")
				if rest == "" || rest[0] == ',' {
					rest = strings.TrimLeft(strings.TrimPrefix(rest, ","), " \t")
					break
				}
				if rest[0] != ';' {
					return "", errors.New("a Link header doesn't parse")
				}
				rest = strings.TrimLeft(rest[1:], " \t")
				i := strings.IndexAny(rest, "=;,")
				if i < 0 || rest[i] != '=' {
					// A parameter without a value.
					if i < 0 {
						i = len(rest)
					}
					rest = rest[i:]
					continue
				}
				name := strings.ToLower(strings.TrimSpace(rest[:i]))
				rest = strings.TrimLeft(rest[i+1:], " \t")
				var value string
				if strings.HasPrefix(rest, `"`) {
					j := strings.IndexByte(rest[1:], '"')
					if j < 0 {
						return "", errors.New("a Link header doesn't parse")
					}
					value, rest = rest[1:1+j], rest[2+j:]
				} else {
					j := strings.IndexAny(rest, ";,")
					if j < 0 {
						j = len(rest)
					}
					value, rest = strings.TrimSpace(rest[:j]), rest[j:]
				}
				if name == "rel" {
					for _, rel := range strings.Fields(value) {
						if strings.EqualFold(rel, "next") {
							isNext = true
						}
					}
				}
			}
			if isNext {
				if found != "" && found != target {
					return "", errors.New("two next pages")
				}
				found = target
			}
		}
	}
	return found, nil
}

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
		// Go's error repeats the URL, which a Link header chose.
		return "", fmt.Errorf("calling GitHub: %s: not a valid request", method)
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
	// Diagnostics name the call without its query, which for a page from a
	// Link header is GitHub's to choose.
	called, _, _ := strings.Cut(strings.TrimPrefix(url, base+"/"), "?")
	where := method + " " + called
	if err != nil {
		// Names the call, never the token or where a redirect went.
		return "", fetch.Error("calling GitHub: "+where, err)
	}
	defer resp.Body.Close()
	// The status decides first: a refusal is one whatever its body.
	if resp.StatusCode < 200 || resp.StatusCode > 299 {
		return "", &Error{Method: method, Path: called, Status: resp.StatusCode}
	}
	payload, err := io.ReadAll(io.LimitReader(resp.Body, maxAnswer+1))
	if err != nil {
		return "", fetch.Error("reading GitHub's answer to "+where, err)
	}
	if len(payload) > maxAnswer {
		return "", fmt.Errorf("GitHub's answer to %s is too large", where)
	}
	if out != nil {
		// An answer that was expected must be there: an empty body isn't
		// an empty result.
		if len(bytes.TrimSpace(payload)) == 0 {
			return "", fmt.Errorf("GitHub's answer to %s is empty", where)
		}
		if err := json.Unmarshal(payload, out); err != nil {
			return "", fmt.Errorf("GitHub's answer to %s: %w", where, err)
		}
	}
	// Every Link header counts: a next page in a second one is still a
	// page. A next page anywhere but the API is an error, not the end.
	nextPage, err := nextLink(resp.Header.Values("Link"))
	if err != nil {
		return "", fmt.Errorf("GitHub's answer to %s: %w", where, err)
	}
	if nextPage != "" && !strings.HasPrefix(nextPage, base+"/") {
		return "", fmt.Errorf("GitHub's answer to %s continues outside its API", where)
	}
	return nextPage, nil
}

// Paginate is every item of a list endpoint, following its pages.
func Paginate[T any](c *Client, path string) ([]T, error) {
	sep := "?"
	if strings.Contains(path, "?") {
		sep = "&"
	}
	var items []T
	url := path + sep + "per_page=100"
	// Diagnostics name the listing and the page's number, never a URL from
	// an answer.
	seen := map[string]bool{}
	for n := 1; url != ""; n++ {
		if seen[url] {
			return nil, fmt.Errorf("GitHub's listing of %s repeats a page", path)
		}
		seen[url] = true
		var raw json.RawMessage
		var err error
		url, err = c.Request(http.MethodGet, url, nil, &raw)
		if err != nil {
			return nil, err
		}
		// Every page is a list; null or anything else is an incomplete
		// listing, not an empty one.
		if !bytes.HasPrefix(bytes.TrimSpace(raw), []byte("[")) {
			return nil, fmt.Errorf("page %d of GitHub's listing of %s is not a list", n, path)
		}
		var page []T
		if err := json.Unmarshal(raw, &page); err != nil {
			return nil, fmt.Errorf("page %d of GitHub's listing of %s: %w", n, path, err)
		}
		items = append(items, page...)
	}
	return items, nil
}
