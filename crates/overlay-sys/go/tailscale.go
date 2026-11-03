// Tailscale half of the overlay shim (see common.go for conventions).
//
// Wraps tailscale.com/tsnet: a userspace Tailscale node with its own identity
// (state in its own directory). Works with Tailscale's service or a
// self-hosted Headscale server (ControlURL).
package main

/*
#include <stdlib.h>
*/
import "C"

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/netip"
	"strings"
	"sync"
	"time"

	"tailscale.com/envknob"
	"tailscale.com/ipn"
	"tailscale.com/tsnet"
)

// DefaultControlURL is Tailscale's own coordination server.
const defaultControlURL = "https://controlplane.tailscale.com"

type tsNode struct {
	srv        *tsnet.Server
	controlURL string

	mu      sync.Mutex
	lastErr string // last critical error from the backend (e.g. a bad auth key)
	cancel  context.CancelFunc
}

var tsNodes = map[C.int]*tsNode{}

func getTS(h C.int) (*tsNode, error) {
	mu.Lock()
	defer mu.Unlock()
	n, ok := tsNodes[h]
	if !ok {
		return nil, errors.New("invalid Tailscale node handle")
	}
	return n, nil
}

type tsOptions struct {
	Hostname string `json:"hostname"`
	// StateDir holds tailscaled.state (this node's keys and login).
	StateDir string `json:"state_dir"`
	// ControlURL: empty for Tailscale, or a Headscale URL.
	ControlURL string `json:"control_url"`
	// AuthKey joins without a browser (only used when there's no saved login).
	AuthKey string `json:"auth_key"`
	// LogFD receives tsnet's logs (-1 discards them).
	LogFD int `json:"log_fd"`
}

//export ts_new
func ts_new(optsJSON *C.char, errOut **C.char) C.int {
	var o tsOptions
	if err := json.Unmarshal([]byte(C.GoString(optsJSON)), &o); err != nil {
		return fail(errOut, fmt.Errorf("bad options: %w", err))
	}
	if o.StateDir == "" {
		return fail(errOut, errors.New("no state directory"))
	}
	// Don't upload logs to Tailscale; they go to PhoneBridge's own log only.
	envknob.SetNoLogsNoSupport()

	w := logWriter(o.LogFD, "tailscale-log")
	logger := log.New(w, "", 0)
	if o.LogFD >= 0 {
		// Some Tailscale packages (e.g. the HTTP proxy lookup) use Go's
		// global logger; send that to PhoneBridge's log too.
		log.SetOutput(w)
		log.SetFlags(0)
	}
	logf := func(format string, args ...any) { logger.Printf(format, args...) }
	controlURL := o.ControlURL
	if controlURL == "" {
		controlURL = defaultControlURL
	}
	srv := &tsnet.Server{
		Dir:        o.StateDir,
		Hostname:   o.Hostname,
		ControlURL: o.ControlURL,
		AuthKey:    o.AuthKey,
		Logf:       logf,
		UserLogf:   logf,
	}
	mu.Lock()
	defer mu.Unlock()
	h := newHandle()
	tsNodes[h] = &tsNode{srv: srv, controlURL: controlURL}
	return h
}

// ts_start starts the node and waits (up to timeout_ms) until it is running,
// needs a login (with a login URL), needs approval, or reports an error. It
// only fails if the node couldn't start at all; the rest shows in the status.
//
//export ts_start
func ts_start(h C.int, timeoutMs C.int, errOut **C.char) C.int {
	n, err := getTS(h)
	if err != nil {
		return fail(errOut, err)
	}
	if err := n.srv.Start(); err != nil {
		return fail(errOut, err)
	}
	lc, err := n.srv.LocalClient()
	if err != nil {
		return fail(errOut, err)
	}

	// Keep the last critical error for the status (a bad auth key, an
	// expired login...). Runs until ts_stop.
	ctx, cancel := context.WithCancel(context.Background())
	n.mu.Lock()
	n.cancel = cancel
	n.mu.Unlock()
	settled := make(chan struct{}, 1)
	go func() {
		w, err := lc.WatchIPNBus(ctx, ipn.NotifyInitialState)
		if err != nil {
			return
		}
		defer w.Close()
		authKey := n.srv.AuthKey != ""
		for {
			nt, err := w.Next()
			if err != nil {
				return
			}
			done := false
			if nt.ErrMessage != nil {
				n.mu.Lock()
				n.lastErr = *nt.ErrMessage
				n.mu.Unlock()
				done = true
			}
			if nt.State != nil {
				switch *nt.State {
				case ipn.Running:
					n.mu.Lock()
					n.lastErr = ""
					n.mu.Unlock()
					done = true
				case ipn.NeedsMachineAuth, ipn.Stopped:
					done = true
				}
			}
			// With an auth key the backend passes through NeedsLogin on its
			// way to Running, so only a login URL ends the wait without one.
			if nt.BrowseToURL != nil && *nt.BrowseToURL != "" && !authKey {
				done = true
			}
			if done {
				select {
				case settled <- struct{}{}:
				default:
				}
			}
		}
	}()

	select {
	case <-settled:
	case <-time.After(time.Duration(timeoutMs) * time.Millisecond):
	}
	return 0
}

//export ts_stop
func ts_stop(h C.int, errOut **C.char) C.int {
	mu.Lock()
	n, ok := tsNodes[h]
	delete(tsNodes, h)
	mu.Unlock()
	if !ok {
		return 0
	}
	n.mu.Lock()
	if n.cancel != nil {
		n.cancel()
	}
	n.mu.Unlock()
	if err := n.srv.Close(); err != nil && !strings.Contains(err.Error(), "not started") {
		return fail(errOut, err)
	}
	return 0
}

func splitIPs(ips []netip.Addr) (v4, v6 string) {
	for _, ip := range ips {
		if ip.Is4() && v4 == "" {
			v4 = ip.String()
		} else if ip.Is6() && v6 == "" {
			v6 = ip.String()
		}
	}
	return
}

//export ts_status_json
func ts_status_json(h C.int, out **C.char, errOut **C.char) C.int {
	n, err := getTS(h)
	if err != nil {
		return fail(errOut, err)
	}
	lc, err := n.srv.LocalClient()
	if err != nil {
		return fail(errOut, err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	st, err := lc.Status(ctx)
	if err != nil {
		return fail(errOut, err)
	}
	n.mu.Lock()
	lastErr := n.lastErr
	n.mu.Unlock()

	s := statusJSON{
		BackendState:  st.BackendState,
		AuthURL:       st.AuthURL,
		ManagementURL: n.controlURL,
		Health:        st.Health,
		Peers:         make([]peerJSON, 0, len(st.Peer)),
	}
	if lastErr != "" && st.BackendState != ipn.Running.String() {
		s.ManagementError = lastErr
	}
	if st.CurrentTailnet != nil {
		s.NetworkName = st.CurrentTailnet.Name
	}
	s.LocalIP, s.LocalIPv6 = splitIPs(st.TailscaleIPs)
	if st.Self != nil {
		s.LocalFQDN = st.Self.DNSName
		s.LocalPubKey = string(st.Self.ID)
	}
	// Once running, the peer list comes from the network map.
	s.Synced = st.BackendState == ipn.Running.String() && st.Self != nil

	for _, p := range st.Peer {
		// Mullvad exit nodes aren't devices on the tailnet.
		if p.Location != nil && p.ExitNodeOption {
			continue
		}
		pj := peerJSON{
			ID:            string(p.ID),
			OS:            p.OS,
			FQDN:          p.DNSName,
			LastHandshake: unixOrZero(p.LastHandshake),
			BytesRx:       p.RxBytes,
			BytesTx:       p.TxBytes,
		}
		if p.DNSName == "" {
			pj.Hostname = p.HostName
		}
		pj.IP, pj.IPv6 = splitIPs(p.TailscaleIPs)
		switch {
		case !p.Online:
			pj.ConnStatus = "Idle"
		case p.CurAddr != "":
			pj.ConnStatus = "Connected"
			pj.RemoteEndpoint = p.CurAddr
		case p.PeerRelay != "":
			pj.ConnStatus = "Connected"
			pj.Relayed = true
			pj.RelayAddress = "peer relay " + p.PeerRelay
		case p.Active && p.Relay != "":
			pj.ConnStatus = "Connected"
			pj.Relayed = true
			pj.RelayAddress = "DERP " + p.Relay
		default:
			// Online, but no traffic yet (Tailscale connects on demand).
			pj.ConnStatus = "Online"
		}
		s.Peers = append(s.Peers, pj)
	}
	data, err := json.Marshal(s)
	if err != nil {
		return fail(errOut, err)
	}
	putString(out, string(data))
	return 0
}

// ts_login asks for a fresh login URL (it then shows up as auth_url in the
// status). Used after a logout or an expired login.
//
//export ts_login
func ts_login(h C.int, errOut **C.char) C.int {
	n, err := getTS(h)
	if err != nil {
		return fail(errOut, err)
	}
	lc, err := n.srv.LocalClient()
	if err != nil {
		return fail(errOut, err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	if err := lc.StartLoginInteractive(ctx); err != nil {
		return fail(errOut, err)
	}
	return 0
}

// ts_logout logs this node out at the control server (it disappears from the
// admin console's active devices) and forgets the login.
//
//export ts_logout
func ts_logout(h C.int, errOut **C.char) C.int {
	n, err := getTS(h)
	if err != nil {
		return fail(errOut, err)
	}
	lc, err := n.srv.LocalClient()
	if err != nil {
		return fail(errOut, err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	if err := lc.Logout(ctx); err != nil {
		return fail(errOut, err)
	}
	return 0
}

// ts_dial opens a TCP connection through Tailscale and hands back one end of
// a socketpair (close-on-exec) piped to it.
//
//export ts_dial
func ts_dial(h C.int, network, addr *C.char, timeoutMs C.int, fdOut *C.int, errOut **C.char) C.int {
	n, err := getTS(h)
	if err != nil {
		return fail(errOut, err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Duration(timeoutMs)*time.Millisecond)
	defer cancel()
	conn, err := n.srv.Dial(ctx, C.GoString(network), C.GoString(addr))
	if err != nil {
		return fail(errOut, err)
	}
	fd, err := connToFD(conn)
	if err != nil {
		return fail(errOut, err)
	}
	*fdOut = fd
	return 0
}
