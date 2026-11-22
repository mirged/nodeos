#!/usr/bin/env python3
"""A tiny IRC server for the kirc test.

Not a server anyone should use: it registers one client, answers PING, joins
whatever channel is asked for, echoes a PRIVMSG back to the channel, and closes
the connection on QUIT. That is exactly the traffic `make test-kirc` needs to
prove kirc's registration, channel and message paths over NodeOS's sockets.

    scripts/lib/ircd.py <port> <log file>

Every line is appended to the log as `C> ...` (from the client) or `S> ...`
(what the server sent), which is what the test greps.
"""
import socket
import sys
import threading

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 6667
LOG = sys.argv[2] if len(sys.argv) > 2 else "/tmp/ircd.log"
lock = threading.Lock()


def log(line):
    with lock:
        with open(LOG, "a") as f:
            f.write(line + "\n")


def send(conn, line):
    log("S> " + line)
    conn.sendall(line.encode() + b"\r\n")


def serve_client(conn, addr):
    log("C> [connected from %s:%d]" % addr)
    buffer = b""
    nick = "kirc"
    user = "kirc"
    registered = False
    while True:
        data = conn.recv(4096)
        if not data:
            break
        buffer += data
        while b"\r\n" in buffer or b"\n" in buffer:
            if b"\r\n" in buffer:
                line, buffer = buffer.split(b"\r\n", 1)
            else:
                line, buffer = buffer.split(b"\n", 1)
            text = line.decode("utf-8", "replace")
            log("C> " + text)
            command, _, rest = text.partition(" ")
            command = command.upper()

            if command == "NICK":
                nick = rest.strip().split(" ")[0]
            elif command == "USER":
                user = rest.split(" ")[0]
                if not registered:
                    registered = True
                    send(conn, ":irc.test 001 %s :Welcome to the test network, %s" % (nick, nick))
                    send(conn, ":irc.test 002 %s :Your host is irc.test, running a stub" % nick)
                    send(conn, ":irc.test 003 %s :This server was written for one test" % nick)
                    send(conn, ":irc.test 004 %s irc.test stub o o" % nick)
                    send(conn, ":irc.test 005 %s CHANTYPES=# PREFIX=(ov)@+ :are supported" % nick)
                    send(conn, ":irc.test 375 %s :- irc.test message of the day" % nick)
                    send(conn, ":irc.test 372 %s :- NodeOS says hello" % nick)
                    send(conn, ":irc.test 376 %s :End of /MOTD command." % nick)
            elif command == "PING":
                send(conn, ":irc.test PONG irc.test :" + rest)
            elif command == "JOIN":
                channel = rest.split(" ")[0]
                send(conn, ":%s!%s@nodeos JOIN %s" % (nick, user, channel))
                send(conn, ":irc.test 353 %s = %s :%s" % (nick, channel, nick))
                send(conn, ":irc.test 366 %s %s :End of /NAMES list." % (nick, channel))
                # The client is told something only the network can produce, so
                # a test can see that a server line reached its screen.
                send(conn, ":irc.test NOTICE %s :kirc-test-welcome" % nick)
            elif command == "PRIVMSG":
                target, _, message = rest.partition(" ")
                # IRC trailing parameters are introduced by a colon; the
                # message itself is what follows it.
                if message.startswith(":"):
                    message = message[1:]
                # Echo it back: the client shows what it sent only if it also
                # reads the socket, which is the path under test.
                send(conn, ":%s!%s@nodeos PRIVMSG %s :%s" % (nick, user, target, message))
            elif command == "PART":
                send(conn, ":%s!%s@nodeos PART %s" % (nick, user, rest.split(" ")[0]))
            elif command == "QUIT":
                send(conn, "ERROR :Closing Link: %s (Quit)" % nick)
                conn.close()
                log("C> [disconnected]")
                return
    conn.close()
    log("C> [disconnected]")


def main():
    open(LOG, "w").close()
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("0.0.0.0", PORT))
    server.listen(4)
    log("S> [listening on %d]" % PORT)
    while True:
        conn, addr = server.accept()
        threading.Thread(target=serve_client, args=(conn, addr), daemon=True).start()


if __name__ == "__main__":
    main()
