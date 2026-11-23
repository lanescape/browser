// Package stop ends the tool, and everything it started, when it's
// signalled.
//
// GitHub signals a cancelled or timed-out step's own process, which is the
// tool when the step execs it, and nothing it started: SIGINT, then SIGTERM.
// Go's default would end the tool at the first and leave ninja, Cargo, go
// test or a suite running on. So the tool keeps SIGINT, SIGTERM and SIGHUP.
// The first closes admission: each command starts through Start, Run or
// Output, which refuse from then on, though one admitted before may still
// start, and the clean exit waits for it to settle; and requests made with
// Context are ended. The tool passes each signal on, by PID, to every
// process descended from it, whatever process group or session it's in
// (ninja gives each of its commands a group of its own), and keeps looking
// for new ones, which it signals too. It exits, as Exit says, once two
// scans in a row find none left, or, after Grace, once it has killed them
// all, or tried to for killFor.
//
// It's made for where CI and CD run (ci/README.md): Linux 6.12 or later and
// macOS 26.2 or later, with the tool its step's own process, run as an
// unprivileged user, and /proc its own PID namespace's.
//
// On Linux the tool is its descendants' subreaper, so one whose parent has
// ended is still its descendant, and becomes its child, which the kernel
// tells it of (noChildren), so that a scan that misses a process as its
// parent ends isn't taken for the end. It signals each process through a
// pidfd, opened once a scan has found it and checked against the start
// time the scan read, which the kernel keeps in clock ticks: from then on
// no later process given the same PID can be signalled, and before then
// only one started in the same tick. macOS has neither: there the tool
// covers what descends from the processes it has seen, found by scanning
// again every scanEvery, and checks a process's start time just before
// signalling it. So on macOS a process that leaves the tree, its parent
// ending, between two scans, or before the first signal, is out of reach,
// and one whose parent's PID went to one of the tool's processes as a scan
// read it could be taken for the tool's. The commands the tool started it
// also reaches through their handles, should a scan miss them: on Linux
// through their pidfds, which a later process given the PID can't take
// over, and on macOS by PID, best effort. Out of reach everywhere is
// what runs as another user, such as sudo's commands. Each CI job's machine
// is its own, and discarded after it.
package stop

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"net/http"
	"os"
	"os/exec"
	"os/signal"
	"sync"
	"syscall"
	"time"
)

// Grace is how long, after the first signal, what the tool started has to
// end before it's killed. GitHub sends SIGTERM 7.5 s after SIGINT.
var Grace = 5 * time.Second

const (
	// killFor bounds the kill at the grace's end.
	killFor = 2 * time.Second
	// scanEvery is how often the tool looks for new processes to stop
	// once it's stopping.
	scanEvery = 50 * time.Millisecond
)

// process is one of the machine's processes, as a listing gives it.
type process struct {
	pid, ppid int
	// start tells a process from a later one given the same PID.
	start int64
	// ended is a zombie, waiting to be reaped.
	ended bool
}

// identity is a process, whatever its parent.
type identity struct {
	pid   int
	start int64
}

func (p process) id() identity { return identity{p.pid, p.start} }

var (
	// mu guards whether the tool is stopping, and what it started, so
	// that nothing starts once it is.
	mu   sync.Mutex
	got  syscall.Signal
	last syscall.Signal
	// event counts the signals received; told records, by process, the
	// last event it was sent.
	event   int
	started = map[identity]bool{}
	// procs are the commands started, which can be signalled without a
	// listing; launching counts those starting.
	procs     []*os.Process
	launching int
	running   bool

	// scanMu orders scans and their signals, and guards seen and told.
	scanMu sync.Mutex
	// seen are the processes the tool is to stop that it has found, and
	// told the last signal each was sent.
	seen = map[identity]bool{}
	told = map[identity]int{}
	// toldProc is told for the commands' handles.
	toldProc = map[*os.Process]int{}

	ctx, cancel = context.WithCancel(context.Background())
)

// Init starts watching for SIGINT, SIGTERM and SIGHUP. The tool calls it
// first, before starting anything.
func Init() {
	mu.Lock()
	defer mu.Unlock()
	if running {
		return
	}
	running = true
	if err := subreaper(); err != nil {
		fmt.Fprintln(os.Stderr, "stop: not its descendants' subreaper:", err)
	}
	// So that a write to a pipe whose reader has gone, as a cancelled
	// step's log can be, fails, rather than ending the tool before what it
	// started. Caught, not ignored, so that what it starts doesn't inherit
	// it.
	signal.Notify(make(chan os.Signal, 1), syscall.SIGPIPE)
	c := make(chan os.Signal, 8)
	signal.Notify(c, syscall.SIGINT, syscall.SIGTERM, syscall.SIGHUP)
	go watch(c)
}

func watch(c <-chan os.Signal) {
	for s := range c {
		sig := s.(syscall.Signal)
		// The signal is recorded and passed on under scanMu, so that the
		// tracker can't pass it on first, and it isn't sent twice.
		scanMu.Lock()
		mu.Lock()
		first := got == 0
		if first {
			got = sig
			cancel()
		}
		last = sig
		event++
		mu.Unlock()
		if first {
			deadline := time.Now().Add(Grace)
			go func() {
				time.Sleep(time.Until(deadline))
				killAll()
			}()
		}
		_, n, err := passLocked(sig, true)
		scanMu.Unlock()
		if first {
			// Not while signalling: a full pipe could hold it up.
			go report(sig, n, err)
			go track()
		}
	}
}

func report(sig syscall.Signal, n int, err error) {
	line := fmt.Sprintf("stopping: %v; signalled %d processes it started", sig, n)
	if err != nil {
		line += fmt.Sprintf(", and couldn't list every process: %v", err)
	}
	fmt.Fprintln(os.Stderr, line)
}

// track looks for new processes to stop until the tool exits, and sends
// each the last signal.
func track() {
	for {
		time.Sleep(scanEvery)
		pass(0, false)
	}
}

// pass scans, and sends sig, or with 0 the last signal received, to each
// process the tool is to stop, or with again false only to those it hasn't
// yet sent the last signal received. It's how many processes it found, and
// how many it signalled.
func pass(sig syscall.Signal, again bool) (live, sent int, err error) {
	scanMu.Lock()
	defer scanMu.Unlock()
	return passLocked(sig, again)
}

// passLocked is pass, with scanMu held.
func passLocked(sig syscall.Signal, again bool) (live, sent int, err error) {
	// Under scanMu, so that a later signal's pass can't be overtaken.
	mu.Lock()
	ev := event
	if sig == 0 {
		sig = last
	}
	ps := append([]*os.Process{}, procs...)
	mu.Unlock()
	targets, err := scan()
	listed := map[int]bool{}
	for _, p := range targets {
		listed[p.pid] = true
		if !again && told[p.id()] == ev {
			continue
		}
		if send(p, sig) == nil {
			if sig != syscall.SIGKILL {
				told[p.id()] = ev
			}
			sent++
		}
	}
	// The commands it started that the listing didn't have, through their
	// handles: on Linux, pidfds, which a later process given the PID can't
	// take over. One that has ended, and that exec hasn't collected, is
	// still there.
	unlisted := 0
	for _, p := range ps {
		if listed[p.Pid] || (!again && toldProc[p] == ev) {
			continue
		}
		if p.Signal(sig) == nil {
			unlisted++
			if sig != syscall.SIGKILL {
				toldProc[p] = ev
			}
			sent++
		}
	}
	return len(targets) + unlisted, sent, err
}

// settled is whether no command is starting.
func settled() bool {
	mu.Lock()
	defer mu.Unlock()
	return launching == 0
}

// killAll kills what's left at the grace's end, scanning again until two
// complete scans in a row find nothing, then exits. It exits at killFor's
// end regardless, saying so.
func killAll() {
	time.AfterFunc(killFor, func() {
		say(fmt.Sprintf("stopping: couldn't confirm that everything it started ended within %v of killing it", killFor))
		os.Exit(Status())
	})
	clean := 0
	for clean < 2 {
		live, _, err := pass(syscall.SIGKILL, true)
		if err == nil && live == 0 && settled() && noChildren() {
			clean++
		} else {
			clean = 0
		}
		time.Sleep(20 * time.Millisecond)
	}
	os.Exit(Status())
}

// say writes a line as the tool exits, giving it up after a moment, as a
// full pipe would hold it.
func say(line string) {
	done := make(chan struct{})
	go func() {
		fmt.Fprintln(os.Stderr, line)
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(100 * time.Millisecond):
	}
}

// scan lists what the tool is to stop: every live process descended from
// it, or from one it found before, which it remembers. A process that has
// ended, if the tool adopted it, it reaps. With an error, the listing was
// incomplete, and the list may be short.
func scan() ([]process, error) {
	all, err := processes()
	self := os.Getpid()
	mu.Lock()
	ours := make(map[identity]bool, len(started))
	for id := range started {
		ours[id] = true
	}
	// While a command is starting, a child that has ended may be it, not
	// yet recorded: nothing is reaped then.
	mayReap := launching == 0
	mu.Unlock()

	children := map[int][]process{}
	alive := map[identity]bool{}
	for _, p := range all {
		children[p.ppid] = append(children[p.ppid], p)
		alive[p.id()] = true
		// One the tool adopted is its to reap; one it started, exec's.
		if mayReap && p.ended && p.ppid == self && !ours[p.id()] {
			reap(p.pid)
		}
	}
	roots := []int{self}
	for id := range seen {
		if alive[id] {
			roots = append(roots, id.pid)
		} else if err == nil {
			delete(seen, id)
		}
	}
	var out []process
	in := map[int]bool{}
	add := func(p process) {
		if !in[p.pid] && p.pid != self && !p.ended {
			in[p.pid] = true
			seen[p.id()] = true
			out = append(out, p)
		}
	}
	for _, p := range all {
		if seen[p.id()] {
			add(p)
		}
	}
	// A link read from an earlier process given a parent's PID isn't the
	// tool's (linked).
	byPID := map[int]process{}
	for _, p := range all {
		byPID[p.pid] = p
	}
	walked := map[int]bool{}
	queue := append([]int{}, roots...)
	for _, p := range out {
		queue = append(queue, p.pid)
	}
	for len(queue) > 0 {
		pid := queue[0]
		queue = queue[1:]
		if walked[pid] {
			continue
		}
		walked[pid] = true
		parent, listed := byPID[pid]
		for _, p := range children[pid] {
			if pid != self && listed && !linked(parent, p) {
				continue
			}
			add(p)
			queue = append(queue, p.pid)
		}
	}
	return out, err
}

// Start starts cmd, unless the tool is stopping. A start admitted before
// the first signal goes ahead, not holding up the stop: the tool's clean
// exit waits for it to settle, and a scan finds what it started.
func Start(cmd *exec.Cmd) error {
	mu.Lock()
	if got != 0 {
		mu.Unlock()
		return ErrStopping
	}
	launching++
	mu.Unlock()
	err := cmd.Start()
	if afterLaunch != nil {
		afterLaunch(cmd)
	}
	var start int64
	if err == nil {
		// Read before exec collects its status, so always there.
		start = startOf(cmd.Process.Pid)
	}
	mu.Lock()
	defer mu.Unlock()
	launching--
	if err != nil {
		return err
	}
	procs = append(procs, cmd.Process)
	// By its start time too, so that a later process given its PID, which
	// the tool might adopt, isn't taken for it.
	started[identity{cmd.Process.Pid, start}] = true
	return nil
}

// afterLaunch, if set, is called as a command has started, before it's
// recorded, for tests.
var afterLaunch func(*exec.Cmd)

// Run is cmd.Run, unless the tool is stopping.
func Run(cmd *exec.Cmd) error {
	if err := Start(cmd); err != nil {
		return err
	}
	return cmd.Wait()
}

// Output is cmd.Output, unless the tool is stopping, except that an
// *exec.ExitError's Stderr is never set.
func Output(cmd *exec.Cmd) ([]byte, error) {
	if cmd.Stdout != nil {
		return nil, errors.New("stop: Stdout already set")
	}
	var out bytes.Buffer
	cmd.Stdout = &out
	err := Run(cmd)
	return out.Bytes(), err
}

// EndGroup kills what's left of the process group a command led, as its
// command ends or times out, unless the tool is stopping: then what's in it
// has the grace to end, as everything else the tool started has, and the
// grace's kill reaches what of it is still the tool's. It decides under
// the lock that records a signal, so it never kills what a signal has
// given the grace, and it never signals a group once the tool is
// stopping, when the group's ID could be another's by now.
func EndGroup(pgid int) error {
	mu.Lock()
	defer mu.Unlock()
	if got != 0 {
		return nil
	}
	return syscall.Kill(-pgid, syscall.SIGKILL)
}

// Exit ends the tool. Until it has been signalled, that's with code. Once it
// has, it's with 128 and the first signal, as a shell gives it, when two
// complete scans in a row find nothing it started, or after the grace,
// once it has killed them all, or tried to for killFor.
func Exit(code int) {
	mu.Lock()
	if got == 0 {
		// Under the lock, so that no signal is taken in between.
		os.Exit(code)
	}
	mu.Unlock()
	clean := 0
	for {
		scanMu.Lock()
		targets, err := scan()
		scanMu.Unlock()
		if err == nil && len(targets) == 0 && settled() && noChildren() {
			clean++
			if clean == 2 {
				os.Exit(Status())
			}
		} else {
			clean = 0
		}
		time.Sleep(scanEvery)
	}
}

// Context is done once the tool has been signalled, ending requests in
// flight and stopping new ones.
func Context() context.Context { return ctx }

// Sleep sleeps for d, or until the tool has been signalled.
func Sleep(d time.Duration) {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-t.C:
	case <-ctx.Done():
	}
}

// Get is client.Get, ended once the tool is stopping. Its errors are
// client.Do's, which name the URL only within a *url.Error.
func Get(client *http.Client, url string) (*http.Response, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, errors.New("not a valid request")
	}
	return client.Do(req)
}

// ErrStopping is the error of whatever won't start once the tool is
// stopping.
var ErrStopping = errors.New("stopping: the tool was signalled")

// Err is ErrStopping once the tool has been signalled.
func Err() error {
	if Signal() != 0 {
		return ErrStopping
	}
	return nil
}

// Signal is the first signal the tool received, or 0.
func Signal() syscall.Signal {
	mu.Lock()
	defer mu.Unlock()
	return got
}

// Status is the exit status for having been stopped, 128 and the first
// signal, or 1 if there was none.
func Status() int {
	mu.Lock()
	defer mu.Unlock()
	if got == 0 {
		return 1
	}
	return 128 + int(got)
}
