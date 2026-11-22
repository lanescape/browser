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
	"slices"
	"sort"
	"strings"
	"time"
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

// Extract reads a tar into destination with the rules of Python's data
// filter. Every name, and every link's target, must resolve inside
// destination through the links already there, those from an earlier
// extraction included; absolute names and links, device files and other
// special members are refused. Files lose setuid, setgid, sticky and group
// and other write bits, lose every executable bit unless the owner's is set,
// and are always readable and writable by their owner. Directories and links
// take default modes. All writes go through an os.Root, so nothing can be
// written outside destination even if a check missed a path.
func Extract(r io.Reader, destination string) error {
	if err := os.MkdirAll(destination, 0o777); err != nil {
		return err
	}
	root, err := os.OpenRoot(destination)
	if err != nil {
		return err
	}
	defer root.Close()
	type dirTime struct {
		name  string
		mtime time.Time
	}
	var dirs []dirTime
	tr := tar.NewReader(r)
	for {
		h, err := tr.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
		if path.IsAbs(h.Name) {
			return fmt.Errorf("refusing %q: its name is absolute", h.Name)
		}
		// Check where the name resolves through the tree, then write at the
		// name as spelled, as tarfile does: an os.Root resolves it the same
		// way. Cleaning a name first would hide a ".." after a link.
		name := strings.TrimRight(h.Name, "/")
		if name == "" {
			name = "."
		}
		real, ok := resolve(root, name)
		if !ok || real == "" && h.Typeflag != tar.TypeDir {
			return fmt.Errorf("refusing %q: it leaves the destination", h.Name)
		}
		switch h.Typeflag {
		case tar.TypeSymlink, tar.TypeLink:
			if path.IsAbs(h.Linkname) {
				return fmt.Errorf("refusing %q: its link is absolute", h.Name)
			}
			from := h.Linkname // a hard link's target is relative to destination
			if h.Typeflag == tar.TypeSymlink {
				from = parent(name) + "/" + h.Linkname
			}
			target, ok := resolve(root, from)
			if !ok {
				return fmt.Errorf("refusing %q: its link leaves the destination", h.Name)
			}
			if err := root.MkdirAll(parent(name), 0o777); err != nil {
				return err
			}
			if err := removeFile(root, name); err != nil {
				return err
			}
			if h.Typeflag == tar.TypeSymlink {
				if err := root.Symlink(path.Clean(h.Linkname), name); err != nil {
					return err
				}
				continue
			}
			if target == "" {
				return fmt.Errorf("refusing %q: it links to the destination", h.Name)
			}
			if err := root.Link(h.Linkname, name); err != nil {
				return err
			}
			if err := root.Chmod(name, fileMode(h.Mode)); err != nil {
				return err
			}
			if err := root.Chtimes(name, h.ModTime, h.ModTime); err != nil {
				return err
			}
		case tar.TypeDir:
			if err := root.MkdirAll(name, 0o777); err != nil {
				return err
			}
			dirs = append(dirs, dirTime{name, h.ModTime})
		case tar.TypeReg, tar.TypeRegA:
			if err := root.MkdirAll(parent(name), 0o777); err != nil {
				return err
			}
			mode := fileMode(h.Mode)
			f, err := root.OpenFile(name, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, mode)
			if err != nil {
				return err
			}
			_, err = io.Copy(f, tr)
			if cerr := f.Close(); err == nil {
				err = cerr
			}
			if err != nil {
				return err
			}
			if err := root.Chmod(name, mode); err != nil {
				return err
			}
			if err := root.Chtimes(name, h.ModTime, h.ModTime); err != nil {
				return err
			}
		default:
			return fmt.Errorf("refusing %q: not a file, directory or link", h.Name)
		}
	}
	// Like tarfile, set directories' times last, deepest first, once
	// nothing more will be written in them.
	for i := len(dirs) - 1; i >= 0; i-- {
		if err := root.Chtimes(dirs[i].name, dirs[i].mtime, dirs[i].mtime); err != nil {
			return err
		}
	}
	return nil
}

// parent is the directory part of a slash-separated name, as spelled.
func parent(name string) string {
	if i := strings.LastIndex(name, "/"); i > 0 {
		return name[:i]
	}
	return "."
}

// fileMode is the mode Python's data filter gives a file or hard link.
func fileMode(archived int64) fs.FileMode {
	mode := fs.FileMode(archived) & 0o755
	if mode&0o100 == 0 {
		mode &^= 0o111
	}
	return mode | 0o600
}

// removeFile removes what is at name, unless it is a directory or absent,
// so that a link can take its place.
func removeFile(root *os.Root, name string) error {
	info, err := root.Lstat(name)
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	if err != nil {
		return err
	}
	if info.IsDir() {
		return fmt.Errorf("%s is a directory", name)
	}
	return root.Remove(name)
}

// resolve returns name, a slash-separated path relative to root, with every
// link along it resolved, as os.path.realpath does, and whether it stays
// inside root. Missing components resolve lexically. The empty path is root
// itself.
func resolve(root *os.Root, name string) (string, bool) {
	var done []string
	todo := strings.Split(name, "/")
	links := 0
	for len(todo) > 0 {
		c := todo[0]
		todo = todo[1:]
		switch c {
		case "", ".":
			continue
		case "..":
			if len(done) == 0 {
				return "", false
			}
			done = done[:len(done)-1]
			continue
		}
		p := path.Join(append(slices.Clone(done), c)...)
		info, err := root.Lstat(p)
		if err != nil || info.Mode()&fs.ModeSymlink == 0 {
			done = append(done, c)
			continue
		}
		target, err := root.Readlink(p)
		if links++; err != nil || links > 40 || path.IsAbs(target) {
			return "", false
		}
		todo = append(strings.Split(target, "/"), todo...)
	}
	return path.Join(done...), true
}
