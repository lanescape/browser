package stop

import (
	"syscall"

	"golang.org/x/sys/unix"
)

// zombie is macOS's SZOMB.
const zombie = 5

// subreaper does nothing: macOS has none, so the tool remembers what it
// has seen descended from it instead.
func subreaper() error { return nil }

// processes lists every process from the kernel.
func processes() ([]process, error) {
	all, err := unix.SysctlKinfoProcSlice("kern.proc.all")
	if err != nil {
		return nil, err
	}
	out := make([]process, 0, len(all))
	for i := range all {
		out = append(out, info(&all[i]))
	}
	return out, nil
}

func info(k *unix.KinfoProc) process {
	t := k.Proc.P_starttime
	return process{pid: int(k.Proc.P_pid), ppid: int(k.Eproc.Ppid),
		start: t.Sec*1_000_000 + int64(t.Usec), ended: k.Proc.P_stat == zombie}
}

// send signals p, checking its start time just before, so that a later
// process given its PID is signalled only if it were given it in between.
func send(p process, sig syscall.Signal) error {
	k, err := unix.SysctlKinfoProc("kern.proc.pid", p.pid)
	if err != nil || k == nil || info(k).start != p.start {
		return syscall.ESRCH
	}
	return syscall.Kill(p.pid, sig)
}

// reap does nothing: without a subreaper, the tool adopts nothing.
func reap(int) {}

// startOf is a process's start time, or -1 if it can't be read.
func startOf(pid int) int64 {
	k, err := unix.SysctlKinfoProc("kern.proc.pid", pid)
	if err != nil || k == nil {
		return -1
	}
	return info(k).start
}

// noChildren is true: macOS keeps no list of a process's children beyond
// what a scan reads, so the scans alone say.
func noChildren() bool { return true }

// linked is true: macOS records start times from the wall clock, which can
// step back, so they can't order a parent and its child; a link read from
// an earlier process given the parent's PID is within the macOS limits the
// package states.
func linked(parent, child process) bool { return true }
