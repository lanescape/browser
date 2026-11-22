// Package fetch describes failed HTTP requests without naming where they
// went. A release asset's download is redirected to a signed URL, and Go's
// errors repeat it, query string and all, as they do a malformed Location.
package fetch

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
)

// Describe classifies err, a failed request or a failed read of a
// response, in words that hold no URL.
func Describe(err error) string {
	var (
		ne       net.Error
		dns      *net.DNSError
		op       *net.OpError
		verify   *tls.CertificateVerificationError
		unknown  x509.UnknownAuthorityError
		hostname x509.HostnameError
		record   tls.RecordHeaderError
	)
	switch {
	case errors.Is(err, context.DeadlineExceeded), errors.As(err, &ne) && ne.Timeout():
		return "timed out"
	case errors.As(err, &dns):
		return "name lookup failed"
	case errors.As(err, &verify), errors.As(err, &unknown), errors.As(err, &hostname), errors.As(err, &record):
		return "TLS failed"
	case errors.As(err, &op):
		return "connection failed"
	case errors.Is(err, io.ErrUnexpectedEOF), errors.Is(err, io.EOF):
		return "connection closed early"
	}
	return "request failed"
}

// Error is err as an error that says what failed, such as "downloading
// manifest.json", and how, but not where.
func Error(what string, err error) error {
	return fmt.Errorf("%s: %s", what, Describe(err))
}

// NoReferer is a CheckRedirect that drops the Referer Go would send on a
// redirect: after a release asset's first hop it would be the signed URL,
// told to wherever the next hop goes. It follows at most 10 redirects.
func NoReferer(req *http.Request, via []*http.Request) error {
	if len(via) >= 10 {
		return errors.New("too many redirects")
	}
	req.Header.Del("Referer")
	return nil
}
