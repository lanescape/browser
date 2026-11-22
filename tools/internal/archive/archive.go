// Package archive makes and reads packages: gzip tarballs whose members
// keep their names and modes, compressed with pigz, in parallel.
package archive

import (
	"archive/tar"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"sort"
	"strings"
)

// Pack writes source as a gzip tarball whose single top-level directory is
// arcname.
func Pack(source, archive, arcname string) error {
	out, err := os.Create(archive)
	if err != nil {
		return err
	}
	defer out.Close()
	pigz := exec.Command("pigz", "-6", "-c")
	pigz.Stdout = out
	pigz.Stderr = os.Stderr
	in, err := pigz.StdinPipe()
	if err != nil {
		return err
	}
	if err := pigz.Start(); err != nil {
		return err
	}
	werr := write(tar.NewWriter(in), source, arcname)
	in.Close()
	if err := pigz.Wait(); err != nil {
		return fmt.Errorf("pigz failed: %w", err)
	}
	if werr != nil {
		return werr
	}
	return out.Close()
}

// WriteTree adds a directory to a tar, under arcname, in name order.
func write(tw *tar.Writer, source, arcname string) error {
	if err := WriteTree(tw, source, arcname); err != nil {
		return err
	}
	return tw.Close()
}

// WriteTree adds source, a file or a directory, to tw as arcname.
func WriteTree(tw *tar.Writer, source, arcname string) error {
	info, err := os.Lstat(source)
	if err != nil {
		return err
	}
	link := ""
	if info.Mode()&fs.ModeSymlink != 0 {
		if link, err = os.Readlink(source); err != nil {
			return err
		}
	}
	h, err := tar.FileInfoHeader(info, link)
	if err != nil {
		return err
	}
	h.Name = arcname
	if info.IsDir() {
		h.Name += "/"
	}
	h.Uname, h.Gname = "", ""
	if err := tw.WriteHeader(h); err != nil {
		return err
	}
	switch {
	case info.Mode().IsRegular():
		f, err := os.Open(source)
		if err != nil {
			return err
		}
		_, err = io.Copy(tw, f)
		f.Close()
		return err
	case info.IsDir():
		entries, err := os.ReadDir(source)
		if err != nil {
			return err
		}
		sort.Slice(entries, func(i, j int) bool { return entries[i].Name() < entries[j].Name() })
		for _, e := range entries {
			if err := WriteTree(tw, filepath.Join(source, e.Name()), arcname+"/"+e.Name()); err != nil {
				return err
			}
		}
	}
	return nil
}

// Unpack extracts a gzip tarball into destination, decompressing with pigz,
// and refusing what a package must never hold (Extract).
func Unpack(archive, destination string) error {
	pigz := exec.Command("pigz", "-dc", archive)
	pigz.Stderr = os.Stderr
	out, err := pigz.StdoutPipe()
	if err != nil {
		return err
	}
	if err := pigz.Start(); err != nil {
		return err
	}
	xerr := Extract(out, destination)
	io.Copy(io.Discard, out)
	if err := pigz.Wait(); err != nil {
		return fmt.Errorf("pigz failed: %w", err)
	}
	return xerr
}

// Extract reads a tar into destination, as Python's data filter does: no
// absolute names or names that leave destination, no links that point
// outside it, no device files, and no setuid, setgid or sticky bits.
func Extract(r io.Reader, destination string) error {
	root, err := filepath.Abs(destination)
	if err != nil {
		return err
	}
	tr := tar.NewReader(r)
	for {
		h, err := tr.Next()
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return err
		}
		name := path.Clean(strings.TrimPrefix(h.Name, "./"))
		if path.IsAbs(h.Name) || name == ".." || strings.HasPrefix(name, "../") {
			return fmt.Errorf("refusing %q: it leaves the destination", h.Name)
		}
		target := filepath.Join(root, filepath.FromSlash(name))
		mode := fs.FileMode(h.Mode).Perm()
		switch h.Typeflag {
		case tar.TypeDir:
			if err := os.MkdirAll(target, mode|0o700); err != nil {
				return err
			}
		case tar.TypeReg:
			if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
				return err
			}
			f, err := os.OpenFile(target, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, mode)
			if err != nil {
				return err
			}
			_, err = io.Copy(f, tr)
			f.Close()
			if err != nil {
				return err
			}
			if err := os.Chmod(target, mode); err != nil {
				return err
			}
		case tar.TypeSymlink:
			dest := h.Linkname
			if !path.IsAbs(dest) {
				dest = path.Join(path.Dir(name), dest)
			}
			if path.IsAbs(h.Linkname) || dest == ".." || strings.HasPrefix(path.Clean(dest), "../") {
				return fmt.Errorf("refusing %q: its link leaves the destination", h.Name)
			}
			if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
				return err
			}
			if err := os.Symlink(h.Linkname, target); err != nil {
				return err
			}
		default:
			return fmt.Errorf("refusing %q: not a file, directory or link", h.Name)
		}
	}
}
