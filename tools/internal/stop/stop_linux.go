package stop

import (
	"errors"
	"io/fs"
	"os"
	"strconv"
	"strings"
	"syscall"

	"golang.org/x/sys/unix"
)

// subreaper keeps a descendant whose parent has ended the tool's, so that
// it's still found.
func subreaper() error {
	return unix.Prctl(unix.PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0)
}

// processes reads every process from /proc. One that ends as it's read is
// left out; with any other failure, the list is what could be read, and
// the error says it may be short.
func processes() ([]process, error) {
	entries, err := os.ReadDir("/proc")
	if err != nil {
		return nil, err
	}
	var out []process
	var first error
	for _, e := range entries {
		pid, err := strconv.Atoi(e.Name())
		if err != nil {
			continue
		}
		p, err := stat(pid)
		switch {
		case err == nil:
			out = append(out, p)
		case errors.Is(err, fs.ErrNotExist), errors.Is(err, syscall.ESRCH):
		case first == nil:
			first = err
		}
	}
	return out, first
}

// stat reads a process from /proc/<pid>/stat: pid (comm) state ppid ...,
// with num_threads the 20th field and starttime the 22nd. comm, in
// parentheses, may hold anything, even spaces or ')'.
func stat(pid int) (process, error) {
	data, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/stat")
	if err != nil {
		return process{}, err
	}
	bad := errors.New("stop: /proc/" + strconv.Itoa(pid) + "/stat doesn't parse")
	s := string(data)
	i := strings.LastIndexByte(s, ')')
	if i < 0 {
		return process{}, bad
	}
	f := strings.Fields(s[i+1:])
	if len(f) < 20 {
		return process{}, bad
	}
	ppid, err1 := strconv.Atoi(f[1])
	threads, err2 := strconv.Atoi(f[17])
	start, err3 := strconv.ParseInt(f[19], 10, 64)
	if err1 != nil || err2 != nil || err3 != nil {
		return process{}, bad
	}
	// A zombie leader with threads still running hasn't ended.
	ended := (f[0] == "Z" && threads <= 1) || f[0] == "X"
	return process{pid: pid, ppid: ppid, start: start, ended: ended}, nil
}

// send signals p through a pidfd, once it has checked the pidfd is p's: a
// process that has ended can't be signalled through it, so its PID given
// to another never is.
func send(p process, sig syscall.Signal) error {
	fd, err := unix.PidfdOpen(p.pid, 0)
	if err != nil {
		return err
	}
	defer unix.Close(fd)
	if q, err := stat(p.pid); err != nil || q.start != p.start {
		return syscall.ESRCH
	}
	return unix.PidfdSendSignal(fd, sig, nil, 0)
}

// reap reaps an adopted process that has ended.
func reap(pid int) {
	var ws unix.WaitStatus
	unix.Wait4(pid, &ws, unix.WNOHANG, nil)
}

// startOf is a process's start time, or -1 if it can't be read.
func startOf(pid int) int64 {
	p, err := stat(pid)
	if err != nil {
		return -1
	}
	return p.start
}

// noChildren is whether the tool has no child at all, as waitid says,
// with WNOWAIT, leaving exit statuses to exec. With the subreaper, every
// descendant whose parent has ended becomes its child, so one a scan missed
// as its parent ended is one. A child whose status exec hasn't collected
// yet counts too: the next pass, or the kill's deadline, settles it.
func noChildren() bool {
	var info unix.Siginfo
	err := unix.Waitid(unix.P_ALL, 0, &info, unix.WEXITED|unix.WNOHANG|unix.WNOWAIT|unix.WALL, nil)
	return errors.Is(err, syscall.ECHILD)
}

// linked is whether child, listed with parent's PID as its parent's, may be
// parent's: a child starts no earlier than its parent, in clock ticks since
// boot, so one that started earlier was the child of an earlier process
// given the PID, read before that ended.
func linked(parent, child process) bool { return child.start >= parent.start }
