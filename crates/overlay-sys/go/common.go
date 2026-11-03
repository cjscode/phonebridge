// PhoneBridge's C interface to the two overlay networks it can join:
// NetBird (netbird.go, github.com/netbirdio/netbird/client/embed) and
// Tailscale (tailscale.go, tailscale.com/tsnet).
//
// Built with `go build -buildmode=c-archive` into liboverlay.a and linked into
// the Rust app. Both run in userspace (netstack): no VPN, no system DNS or
// routes, no admin rights. They share one Go runtime, which is why they live
// in one archive (two Go c-archives can't be linked into one program).
//
// Conventions: functions return 0 on success and -1 on error. On error,
// *err_out (when non-NULL) receives a malloc'd message the caller frees with
// ov_free. Strings returned through *_out parameters are also freed with ov_free.
// Connections are handed out as one end of a close-on-exec socketpair.
package main

/*
#include <stdlib.h>
*/
import "C"

import (
	"fmt"
	"io"
	"net"
	"os"
	"sync"
	"syscall"
	"time"
	"unsafe"
)

func main() {}

var (
	// mu guards the handle tables in netbird.go and tailscale.go.
	mu     sync.Mutex
	nextID C.int = 1
)

func newHandle() C.int {
	h := nextID
	nextID++
	return h
}

func fail(errOut **C.char, err error) C.int {
	if errOut != nil {
		*errOut = C.CString(err.Error())
	}
	return -1
}

func putString(out **C.char, s string) {
	if out != nil {
		*out = C.CString(s)
	}
}

//export ov_free
func ov_free(p *C.char) {
	C.free(unsafe.Pointer(p))
}

func errString(err error) string {
	if err == nil {
		return ""
	}
	return err.Error()
}

func unixOrZero(t time.Time) int64 {
	if t.IsZero() {
		return 0
	}
	return t.Unix()
}

// logWriter turns a file descriptor from Rust into a writer (-1 discards).
func logWriter(fd int, name string) io.Writer {
	if fd < 0 {
		return io.Discard
	}
	return os.NewFile(uintptr(fd), name)
}

// connToFD pipes conn to one end of a new socketpair and returns the other end.
func connToFD(conn net.Conn) (C.int, error) {
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	if err != nil {
		conn.Close()
		return -1, fmt.Errorf("socketpair: %w", err)
	}
	syscall.CloseOnExec(fds[0])
	syscall.CloseOnExec(fds[1])
	f := os.NewFile(uintptr(fds[1]), "overlay-conn")
	local, err := net.FileConn(f) // dups the fd
	f.Close()
	if err != nil {
		conn.Close()
		syscall.Close(fds[0])
		return -1, fmt.Errorf("wrapping socketpair: %w", err)
	}
	go pipe(conn, local)
	return C.int(fds[0]), nil
}

type closeWriter interface{ CloseWrite() error }

// pipe copies both ways, half-closing each side when the other finishes.
func pipe(a, b net.Conn) {
	var wg sync.WaitGroup
	cp := func(dst, src net.Conn) {
		defer wg.Done()
		_, _ = io.Copy(dst, src)
		if cw, ok := dst.(closeWriter); ok {
			_ = cw.CloseWrite()
		} else {
			_ = dst.Close()
		}
	}
	wg.Add(2)
	go cp(a, b)
	go cp(b, a)
	wg.Wait()
	a.Close()
	b.Close()
}

// peerJSON and statusJSON are the status shape both networks report, so the
// Rust side parses one format. Fields a network doesn't have stay empty.
type peerJSON struct {
	// ID is stable while the peer keeps its identity (NetBird: WireGuard
	// public key; Tailscale: stable node ID).
	ID       string `json:"pub_key"`
	Hostname string `json:"hostname,omitempty"`
	OS       string `json:"os,omitempty"`
	IP       string `json:"ip"`
	IPv6     string `json:"ipv6"`
	FQDN     string `json:"fqdn"`
	// "Idle", "Connecting" or "Connected".
	ConnStatus     string `json:"conn_status"`
	Relayed        bool   `json:"relayed"`
	LocalEndpoint  string `json:"local_endpoint"`
	RemoteEndpoint string `json:"remote_endpoint"`
	RelayAddress   string `json:"relay_address"`
	LatencyMs      int64  `json:"latency_ms"`
	LastHandshake  int64  `json:"last_handshake_unix"`
	BytesRx        int64  `json:"bytes_rx"`
	BytesTx        int64  `json:"bytes_tx"`
}

type statusJSON struct {
	// Synced is true once the first network map arrived; before that the
	// peer list is incomplete.
	Synced bool `json:"synced"`
	// Tailscale only: NoState, NeedsLogin, NeedsMachineAuth, Starting,
	// Running, Stopped. Empty for NetBird (a started client is running).
	BackendState string `json:"backend_state,omitempty"`
	// Tailscale only: the login URL while BackendState is NeedsLogin.
	AuthURL string `json:"auth_url,omitempty"`
	// Tailscale only: the tailnet's name.
	NetworkName   string `json:"network_name,omitempty"`
	ManagementURL string `json:"management_url"`
	// NetBird only (nil for Tailscale, which reports Health instead).
	ManagementConnected *bool  `json:"management_connected,omitempty"`
	ManagementError     string `json:"management_error,omitempty"`
	SignalConnected     *bool  `json:"signal_connected,omitempty"`
	// Tailscale only: health warnings.
	Health      []string   `json:"health,omitempty"`
	LocalIP     string     `json:"local_ip"`
	LocalIPv6   string     `json:"local_ipv6"`
	LocalFQDN   string     `json:"local_fqdn"`
	LocalPubKey string     `json:"local_pub_key"`
	Peers       []peerJSON `json:"peers"`
}
