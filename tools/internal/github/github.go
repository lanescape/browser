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
	neturl "net/url"
	"os"
	"regexp"
	"strings"
	"time"

	"github.com/lanescape/browser/tools/internal/fetch"
	"github.com/lanescape/browser/tools/internal/stop"
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

// relType is a registered relation type's form (RFC 8288, section 2.1.1).
var relType = regexp.MustCompile(`^[A-Za-z][A-Za-z0-9.-]*$`)

// uriScheme is a URI scheme's form (RFC 3986, section 3.1).
var uriScheme = regexp.MustCompile(`^[A-Za-z][A-Za-z0-9+.-]*$`)

// absoluteURI is whether s is an absolute URI, an extension relation type.
func absoluteURI(s string) bool {
	u, err := neturl.Parse(s)
	return err == nil && uriScheme.MatchString(u.Scheme) && (u.Host != "" || u.Opaque != "")
}

// nextLink is the target of the Link headers' rel="next" link, parsed as
// RFC 8288 has them: links separated by commas, each a <target> followed by
// parameters, whose values may be quoted strings with backslash escapes;
// only a link's first rel counts, and its value is a list of relation
// types. Two different next links, an empty one, or a header that doesn't
// parse is an error rather than a guess.
func nextLink(headers []string) (string, error) {
	bad := errors.New("a Link header doesn't parse")
	found, present := "", false
	for _, header := range headers {
		rest := strings.TrimSpace(header)
		for rest != "" {
			if rest[0] != '<' {
				return "", bad
			}
			end := strings.IndexByte(rest, '>')
			if end < 0 {
				return "", bad
			}
			target := rest[1:end]
			rest = rest[end+1:]
			isNext, sawRel := false, false
			for {
				rest = strings.TrimLeft(rest, " \t")
				if rest == "" || rest[0] == ',' {
					rest = strings.TrimLeft(strings.TrimPrefix(rest, ","), " \t")
					break
				}
				if rest[0] != ';' {
					return "", bad
				}
				rest = strings.TrimLeft(rest[1:], " \t")
				i := strings.IndexAny(rest, "=;,")
				if i < 0 || rest[i] != '=' {
					// A parameter without a value; rel needs one.
					if i < 0 {
						i = len(rest)
					}
					if strings.EqualFold(strings.TrimSpace(rest[:i]), "rel") && !sawRel {
						return "", bad
					}
					rest = rest[i:]
					continue
				}
				name := strings.ToLower(strings.TrimSpace(rest[:i]))
				rest = strings.TrimLeft(rest[i+1:], " \t")
				var value string
				if strings.HasPrefix(rest, `"`) {
					var b strings.Builder
					k := 1
					for ; k < len(rest) && rest[k] != '"'; k++ {
						if rest[k] == '\\' {
							k++
							if k == len(rest) {
								return "", bad
							}
						}
						b.WriteByte(rest[k])
					}
					if k == len(rest) {
						return "", bad
					}
					value, rest = b.String(), rest[k+1:]
				} else {
					j := strings.IndexAny(rest, ";,")
					if j < 0 {
						j = len(rest)
					}
					value, rest = strings.TrimSpace(rest[:j]), rest[j:]
				}
				if name == "rel" && !sawRel {
					sawRel = true
					if strings.TrimSpace(value) == "" {
						return "", bad
					}
					for _, rel := range strings.Fields(value) {
						// A registered type, or a URI: anything else, such
						// as "next?", is a header that didn't parse as
						// meant.
						if !relType.MatchString(rel) && !absoluteURI(rel) {
							return "", bad
						}
						if strings.EqualFold(rel, "next") {
							isNext = true
						}
					}
				}
			}
			// Every link has a relation type: one without is a header
			// that didn't parse as meant.
			if !sawRel {
				return "", bad
			}
			if isNext {
				if target == "" || (present && found != target) {
					return "", errors.New("an empty or second next page")
				}
				found, present = target, true
			}
		}
	}
	return found, nil
}

// Request calls path, such as repos/o/r/git/refs, or a full API URL, with
// body as JSON if not nil, and decodes the answer into out if not nil. It
// returns the next page's URL, if any.
func (c *Client) Request(method, path string, body, out any) (string, error) {
	return c.request(method, path, "", body, out)
}

// request is Request; diagnostics name the call as shown, or, if shown is
// empty, by its path without its query.
func (c *Client) request(method, path, shown string, body, out any) (string, error) {
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
	req, err := http.NewRequestWithContext(stop.Context(), method, url, reader)
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
	// Redirects stay on the API's origin: an answer from anywhere else is
	// not GitHub's, and the token isn't sent there either.
	apiURL, err := neturl.Parse(base)
	if err != nil {
		return "", errors.New("the GitHub API's address doesn't parse")
	}
	bounded := *client
	bounded.CheckRedirect = func(next *http.Request, via []*http.Request) error {
		if next.URL.Scheme != apiURL.Scheme || next.URL.Host != apiURL.Host {
			return errors.New("redirected away from GitHub's API")
		}
		if len(via) >= 10 {
			return errors.New("too many redirects")
		}
		return nil
	}
	resp, err := bounded.Do(req)
	// Diagnostics name the call without its query, which for a page from a
	// Link header is GitHub's to choose.
	called, _, _ := strings.Cut(strings.TrimPrefix(url, base+"/"), "?")
	if shown != "" {
		called = shown
	}
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
		// A later page's address is GitHub's to choose, and could hold
		// anything: it's named by the listing and its number.
		url, err = c.request(http.MethodGet, url, fmt.Sprintf("%s (page %d)", path, n), nil, &raw)
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
