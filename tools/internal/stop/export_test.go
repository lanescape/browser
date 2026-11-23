package stop

// Running is whether pid is a live process, not ended or merely unreaped,
// by the same listing the package uses; an error is the listing's.
func Running(pid int) (bool, error) {
	all, err := processes()
	if err != nil {
		return false, err
	}
	for _, p := range all {
		if p.pid == pid {
			return !p.ended, nil
		}
	}
	return false, nil
}

// StartOf is a process's start time, or -1 if it can't be read.
func StartOf(pid int) int64 { return startOf(pid) }
