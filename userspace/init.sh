#!/bin/sh
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# The first program the kernel starts (/init): a script for BusyBox's shell, which the kernel
# runs through the `#!` line. It checks that the shell works (each check prints one line that
# `cargo xtask test` looks for), then starts an interactive shell on the console, again and
# again: init must never exit (the kernel panics if it does, as Linux does).

echo "init: running as process $$"

/bin/hello
echo "init: /bin/hello exited with status $?"

# Pipes between separate programs (fork, execve, pipe, wait4).
echo "init: pipeline $(echo ochra | cat | tr a-z A-Z)"
echo "init: counted $(seq 1 5 | wc -l) lines"

# Arithmetic, loops and functions inside the shell.
square() {
    echo $(($1 * $1))
}
total=0
for number in 1 2 3; do
    total=$((total + $(square "$number")))
done
echo "init: sum of squares $total"

# Exit statuses, a subshell, a job in the background.
false
echo "init: false gave $?"
(exit 7)
echo "init: subshell gave $?"
(exit 9) &
wait $!
echo "init: background job gave $?"

# Signals: a trap runs on the shell's own signal; a killed child reports its signal.
trap 'echo "init: trap ran for USR1"' USR1
kill -USR1 $$
# `yes` writes forever; when `head` has its lines and exits, SIGPIPE ends `yes`.
echo "init: $(yes ochra | head -n 3 | tr '\n' ' ')from yes"
cat <<EOF
init: here-document with $((6 * 7))
EOF

# Files: a redirect into /tmp, directories, a move, a glob (getdents64), removal.
cd /tmp
echo ochra > note
mkdir -p tree/branch
mv note tree/branch/
ln -s branch/note tree/link
echo "init: files $(cat tree/link) $(echo tree/*) $(ls tree/branch)"
rm -r tree
echo "init: /tmp holds $(ls -a /tmp | wc -l) entries again"
cd /

echo "init: checks done"
while true; do
    echo "init: starting a shell"
    /bin/sh
    echo "init: the shell exited with status $?"
done
