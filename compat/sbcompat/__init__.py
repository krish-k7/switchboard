"""Black-box harness for the Switchboard compatibility suite.

Nothing here imports the code under test. Each component is started from the
switchboard binary (see impl.py and run.py), and tests talk to it only through
its public interfaces: HTTP for the server, line JSON on stdin/stdout for the
helper, native registry files and sockets for Claude sessions, and process
arguments, files, and output for the CLI and bridge.
"""
