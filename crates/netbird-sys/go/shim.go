// PhoneBridge's C interface to the NetBird client (github.com/netbirdio/netbird/client/embed).
//
// Built with `go build -buildmode=c-archive` into libnetbird.a and linked into
// the Rust app. NetBird runs in userspace (netstack): no VPN, no system DNS or
// routes, no admin rights.
//
// Conventions: functions return 0 on success and -1 on error. On error,
// *err_out (when non-NULL) receives a malloc'd message the caller frees with
// nb_free. Strings returned through *_out parameters are also freed with nb_free.
package main

/*
#include <stdlib.h>
*/
import "C"

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"html"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"strings"
	"sync"
	"syscall"
	"time"
	"unsafe"

	"github.com/netbirdio/netbird/client/embed"
	mgm "github.com/netbirdio/netbird/shared/management/client"
	"golang.org/x/oauth2"
	"golang.zx2c4.com/wireguard/wgctrl/wgtypes"
)

func main() {}

var (
	mu      sync.Mutex
	nextID  C.int = 1
	clients       = map[C.int]*embed.Client{}
	flows         = map[C.int]*ssoFlow{}
)

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

//export nb_free
func nb_free(p *C.char) {
	C.free(unsafe.Pointer(p))
}

func getClient(h C.int) (*embed.Client, error) {
	mu.Lock()
	defer mu.Unlock()
	c, ok := clients[h]
	if !ok {
		return nil, errors.New("invalid NetBird client handle")
	}
	return c, nil
}

// ---- client lifecycle -------------------------------------------------------

type newOptions struct {
	DeviceName    string `json:"device_name"`
	ManagementURL string `json:"management_url"`
	// ConfigPath persists the peer's identity (WireGuard private key etc.).
	ConfigPath string `json:"config_path"`
	StatePath  string `json:"state_path"`
	// Exactly one credential: a setup key, an SSO token, or the saved key in ConfigPath.
	SetupKey    string `json:"setup_key"`
	JWTToken    string `json:"jwt_token"`
	UseSavedKey bool   `json:"use_saved_key"`
	// LogFD receives NetBird's logs (-1 discards them).
	LogFD    int    `json:"log_fd"`
	LogLevel string `json:"log_level"`
}

// savedPrivateKey reads the WireGuard private key from NetBird's config file.
func savedPrivateKey(configPath string) (string, error) {
	data, err := os.ReadFile(configPath)
	if err != nil {
		return "", fmt.Errorf("no saved NetBird login (%w)", err)
	}
	var cfg struct {
		PrivateKey string `json:"PrivateKey"`
	}
	if err := json.Unmarshal(data, &cfg); err != nil {
		return "", fmt.Errorf("reading %s: %w", configPath, err)
	}
	if cfg.PrivateKey == "" {
		return "", fmt.Errorf("no private key in %s", configPath)
	}
	return cfg.PrivateKey, nil
}

//export nb_new
func nb_new(optsJSON *C.char, errOut **C.char) C.int {
	var o newOptions
	if err := json.Unmarshal([]byte(C.GoString(optsJSON)), &o); err != nil {
		return fail(errOut, fmt.Errorf("bad options: %w", err))
	}
	var logOut io.Writer = io.Discard
	if o.LogFD >= 0 {
		logOut = os.NewFile(uintptr(o.LogFD), "netbird-log")
	}
	lazy := false // connect to peers eagerly, so their status means something
	wgPort := 0   // random UDP port: never clash with an installed NetBird agent
	opts := embed.Options{
		DeviceName:            o.DeviceName,
		ManagementURL:         o.ManagementURL,
		ConfigPath:            o.ConfigPath,
		StatePath:             o.StatePath,
		LogOutput:             logOut,
		LogLevel:              o.LogLevel,
		DisableClientRoutes:   true, // no routes / exit nodes: we only dial peers
		LazyConnectionEnabled: &lazy,
		WireguardPort:         &wgPort,
	}
	switch {
	case o.SetupKey != "":
		opts.SetupKey = o.SetupKey
	case o.JWTToken != "":
		opts.JWTToken = o.JWTToken
	case o.UseSavedKey:
		key, err := savedPrivateKey(o.ConfigPath)
		if err != nil {
			return fail(errOut, err)
		}
		opts.PrivateKey = key
	default:
		return fail(errOut, errors.New("no NetBird credential given"))
	}
	c, err := embed.New(opts)
	if err != nil {
		return fail(errOut, err)
	}
	mu.Lock()
	defer mu.Unlock()
	h := nextID
	nextID++
	clients[h] = c
	return h
}

// nb_start logs in and brings the engine up. Blocks up to timeout_ms.
//
//export nb_start
func nb_start(h C.int, timeoutMs C.int, errOut **C.char) C.int {
	c, err := getClient(h)
	if err != nil {
		return fail(errOut, err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Duration(timeoutMs)*time.Millisecond)
	defer cancel()
	if err := c.Start(ctx); err != nil {
		return fail(errOut, err)
	}
	return 0
}

// nb_stop stops the engine and releases the handle.
//
//export nb_stop
func nb_stop(h C.int, errOut **C.char) C.int {
	mu.Lock()
	c, ok := clients[h]
	delete(clients, h)
	mu.Unlock()
	if !ok {
		return 0
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := c.Stop(ctx); err != nil && !errors.Is(err, embed.ErrClientNotStarted) {
		return fail(errOut, err)
	}
	return 0
}

// ---- status -----------------------------------------------------------------

type peerJSON struct {
	PubKey         string `json:"pub_key"`
	IP             string `json:"ip"`
	IPv6           string `json:"ipv6"`
	FQDN           string `json:"fqdn"`
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
	// Synced is true once the first network map arrived from management;
	// before that the peer list is incomplete.
	Synced              bool       `json:"synced"`
	ManagementURL       string     `json:"management_url"`
	ManagementConnected bool       `json:"management_connected"`
	ManagementError     string     `json:"management_error"`
	SignalConnected     bool       `json:"signal_connected"`
	LocalIP             string     `json:"local_ip"`
	LocalIPv6           string     `json:"local_ipv6"`
	LocalFQDN           string     `json:"local_fqdn"`
	LocalPubKey         string     `json:"local_pub_key"`
	Peers               []peerJSON `json:"peers"`
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

//export nb_status_json
func nb_status_json(h C.int, out **C.char, errOut **C.char) C.int {
	c, err := getClient(h)
	if err != nil {
		return fail(errOut, err)
	}
	st, err := c.Status()
	if err != nil {
		return fail(errOut, err)
	}
	s := statusJSON{
		ManagementURL:       st.ManagementState.URL,
		ManagementConnected: st.ManagementState.Connected,
		ManagementError:     errString(st.ManagementState.Error),
		SignalConnected:     st.SignalState.Connected,
		LocalIP:             st.LocalPeerState.IP,
		LocalIPv6:           st.LocalPeerState.IPv6,
		LocalFQDN:           st.LocalPeerState.FQDN,
		LocalPubKey:         st.LocalPeerState.PubKey,
		Peers:               make([]peerJSON, 0, len(st.Peers)),
	}
	seen := make(map[string]bool, len(st.Peers))
	for _, p := range st.Peers {
		seen[p.PubKey] = true
		s.Peers = append(s.Peers, peerJSON{
			PubKey:         p.PubKey,
			IP:             p.IP,
			IPv6:           p.IPv6,
			FQDN:           p.FQDN,
			ConnStatus:     p.ConnStatus.String(),
			Relayed:        p.Relayed,
			LocalEndpoint:  p.LocalIceCandidateEndpoint,
			RemoteEndpoint: p.RemoteIceCandidateEndpoint,
			RelayAddress:   p.RelayServerAddress,
			LatencyMs:      p.Latency.Milliseconds(),
			LastHandshake:  unixOrZero(p.LastWireguardHandshake),
			BytesRx:        p.BytesRx,
			BytesTx:        p.BytesTx,
		})
	}
	// The network map is the authoritative peer list; the status recorder only
	// knows peers the engine has started handling. Add any it doesn't know yet.
	if resp, err := c.GetLatestSyncResponse(); err == nil && resp != nil && resp.GetNetworkMap() != nil {
		s.Synced = true
		for _, rp := range resp.GetNetworkMap().GetRemotePeers() {
			key := rp.GetWgPubKey()
			if key == "" || seen[key] {
				continue
			}
			seen[key] = true
			ip := ""
			if ips := rp.GetAllowedIps(); len(ips) > 0 {
				ip = ips[0]
			}
			s.Peers = append(s.Peers, peerJSON{PubKey: key, IP: ip, FQDN: rp.GetFqdn(), ConnStatus: "Idle"})
		}
	}
	data, err := json.Marshal(s)
	if err != nil {
		return fail(errOut, err)
	}
	putString(out, string(data))
	return 0
}

// ---- dialing ----------------------------------------------------------------

// nb_dial opens a TCP connection through NetBird and hands back one end of a
// socketpair (close-on-exec) that is piped to it.
//
//export nb_dial
func nb_dial(h C.int, network, addr *C.char, timeoutMs C.int, fdOut *C.int, errOut **C.char) C.int {
	c, err := getClient(h)
	if err != nil {
		return fail(errOut, err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Duration(timeoutMs)*time.Millisecond)
	defer cancel()
	conn, err := c.Dial(ctx, C.GoString(network), C.GoString(addr))
	if err != nil {
		return fail(errOut, err)
	}
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	if err != nil {
		conn.Close()
		return fail(errOut, fmt.Errorf("socketpair: %w", err))
	}
	syscall.CloseOnExec(fds[0])
	syscall.CloseOnExec(fds[1])
	f := os.NewFile(uintptr(fds[1]), "netbird-conn")
	local, err := net.FileConn(f) // dups the fd
	f.Close()
	if err != nil {
		conn.Close()
		syscall.Close(fds[0])
		return fail(errOut, fmt.Errorf("wrapping socketpair: %w", err))
	}
	go pipe(conn, local)
	*fdOut = C.int(fds[0])
	return 0
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

// ---- browser (SSO) login ----------------------------------------------------

type providerConfig struct {
	ClientID, ClientSecret, Audience, Scope     string
	AuthEndpoint, TokenEndpoint, DeviceEndpoint string
	RedirectURLs                                []string
	UseIDToken, DisablePromptLogin              bool
	LoginFlag                                   uint32
}

type ssoFlow struct {
	kind     string // "pkce" or "device"
	provider providerConfig
	// pkce
	oauth    *oauth2.Config
	state    string
	verifier string
	servers  []*http.Server
	result   chan tokenResult
	// device
	deviceCode string
	interval   time.Duration
	expires    time.Time
	cancel     context.CancelFunc
	ctx        context.Context
}

// How long a browser login may take before we give up.
const ssoTimeout = 10 * time.Minute

type tokenResult struct {
	token string
	err   error
}

type ssoBeginJSON struct {
	ID        int    `json:"id"`
	Kind      string `json:"kind"`
	URL       string `json:"url"`
	UserCode  string `json:"user_code,omitempty"`
	ExpiresIn int    `json:"expires_in"`
}

func randomHex(n int) (string, error) {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return hex.EncodeToString(b), nil
}

func mgmtClient(ctx context.Context, mgmtURL string, key wgtypes.Key) (*mgm.GrpcClient, error) {
	u, err := url.Parse(mgmtURL)
	if err != nil || u.Host == "" {
		return nil, fmt.Errorf("invalid management URL %q", mgmtURL)
	}
	host := u.Host
	if u.Port() == "" {
		if u.Scheme == "http" {
			host += ":80"
		} else {
			host += ":443"
		}
	}
	return mgm.NewClient(ctx, host, key, u.Scheme == "https")
}

// nb_sso_begin asks the management server how to log in and starts a browser
// login. PKCE (browser + localhost callback) is preferred, the device-code flow
// is the fallback. Returns JSON {id, kind, url, user_code, expires_in}.
//
//export nb_sso_begin
func nb_sso_begin(mgmtURL *C.char, out **C.char, errOut **C.char) C.int {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	key, err := wgtypes.GeneratePrivateKey()
	if err != nil {
		return fail(errOut, err)
	}
	client, err := mgmtClient(ctx, C.GoString(mgmtURL), key)
	if err != nil {
		return fail(errOut, fmt.Errorf("can't reach the NetBird management server: %w", err))
	}
	defer client.Close()

	var flow *ssoFlow
	if pk, perr := client.GetPKCEAuthorizationFlow(false); perr == nil {
		pc := pk.GetProviderConfig()
		flow, err = beginPKCE(providerConfig{
			ClientID: pc.GetClientID(), ClientSecret: pc.GetClientSecret(), Audience: pc.GetAudience(),
			Scope: pc.GetScope(), AuthEndpoint: pc.GetAuthorizationEndpoint(), TokenEndpoint: pc.GetTokenEndpoint(),
			RedirectURLs: pc.GetRedirectURLs(), UseIDToken: pc.GetUseIDToken(),
			DisablePromptLogin: pc.GetDisablePromptLogin(), LoginFlag: pc.GetLoginFlag(),
		})
		if err != nil {
			return fail(errOut, err)
		}
	} else if dv, derr := client.GetDeviceAuthorizationFlow(); derr == nil {
		pc := dv.GetProviderConfig()
		flow, err = beginDevice(providerConfig{
			ClientID: pc.GetClientID(), Audience: pc.GetAudience(), Scope: pc.GetScope(),
			TokenEndpoint: pc.GetTokenEndpoint(), DeviceEndpoint: pc.GetDeviceAuthEndpoint(), UseIDToken: pc.GetUseIDToken(),
		})
		if err != nil {
			return fail(errOut, err)
		}
	} else {
		return fail(errOut, fmt.Errorf("this NetBird server offers no browser login (pkce: %v; device: %v). Use a setup key", perr, derr))
	}

	mu.Lock()
	id := nextID
	nextID++
	flows[id] = flow
	mu.Unlock()

	resp := ssoBeginJSON{ID: int(id), Kind: flow.kind, ExpiresIn: int(time.Until(flow.expires).Seconds())}
	if flow.kind == "pkce" {
		resp.URL = flow.authURL()
	} else {
		resp.URL = flow.provider.AuthEndpoint // verification URL (complete, with code)
		resp.UserCode = flow.state            // user code
	}
	data, _ := json.Marshal(resp)
	putString(out, string(data))
	return 0
}

func (f *ssoFlow) authURL() string {
	params := []oauth2.AuthCodeOption{
		oauth2.SetAuthURLParam("code_challenge_method", "S256"),
		oauth2.SetAuthURLParam("code_challenge", codeChallenge(f.verifier)),
		oauth2.SetAuthURLParam("audience", f.provider.Audience),
	}
	if !f.provider.DisablePromptLogin {
		// Values from netbird shared/management/client/common.LoginFlag.
		switch f.provider.LoginFlag {
		case 0: // LoginFlagPromptLogin
			params = append(params, oauth2.SetAuthURLParam("prompt", "login"))
		case 1: // LoginFlagMaxAge0
			params = append(params, oauth2.SetAuthURLParam("max_age", "0"))
		}
	}
	return f.oauth.AuthCodeURL(f.state, params...)
}

func codeChallenge(verifier string) string {
	sum := sha256.Sum256([]byte(verifier))
	return base64.RawURLEncoding.EncodeToString(sum[:])
}

func beginPKCE(p providerConfig) (*ssoFlow, error) {
	state, err := randomHex(24)
	if err != nil {
		return nil, err
	}
	verifier, err := randomHex(64)
	if err != nil {
		return nil, err
	}
	f := &ssoFlow{kind: "pkce", provider: p, state: state, verifier: verifier,
		result: make(chan tokenResult, 1), expires: time.Now().Add(ssoTimeout)}
	f.ctx, f.cancel = context.WithDeadline(context.Background(), f.expires)

	// Listen on the first configured redirect URL whose port is free, on both
	// loopback addresses ("localhost" may resolve to either).
	var lastErr error
	for _, ru := range p.RedirectURLs {
		u, err := url.Parse(ru)
		if err != nil || u.Port() == "" {
			continue
		}
		var listeners []net.Listener
		for _, host := range []string{"127.0.0.1", "::1"} {
			l, err := net.Listen("tcp", net.JoinHostPort(host, u.Port()))
			if err != nil {
				lastErr = err
				continue
			}
			listeners = append(listeners, l)
		}
		if len(listeners) == 0 {
			continue
		}
		f.oauth = &oauth2.Config{
			ClientID:     p.ClientID,
			ClientSecret: p.ClientSecret,
			Endpoint:     oauth2.Endpoint{AuthURL: p.AuthEndpoint, TokenURL: p.TokenEndpoint},
			RedirectURL:  ru,
			Scopes:       strings.Fields(p.Scope),
		}
		mux := http.NewServeMux()
		mux.HandleFunc("/", f.handleCallback)
		for _, l := range listeners {
			srv := &http.Server{Handler: mux, ReadHeaderTimeout: 10 * time.Second}
			f.servers = append(f.servers, srv)
			go srv.Serve(l)
		}
		return f, nil
	}
	return nil, fmt.Errorf("no free local port for the login callback (tried %v): %v", p.RedirectURLs, lastErr)
}

const callbackPage = `<!doctype html><meta charset="utf-8"><title>PhoneBridge</title>
<body style="font-family:-apple-system,sans-serif;text-align:center;padding-top:15vh">
<h2>%s</h2><p>%s</p></body>`

func (f *ssoFlow) handleCallback(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	report := func(res tokenResult) {
		select {
		case f.result <- res:
		default:
		}
	}
	if e := q.Get("error"); e != "" {
		msg := e + ": " + q.Get("error_description")
		fmt.Fprintf(w, callbackPage, "Login failed", html.EscapeString(msg))
		report(tokenResult{err: errors.New(msg)})
		return
	}
	if q.Get("state") != f.state {
		http.Error(w, "invalid state", http.StatusBadRequest)
		return
	}
	tok, err := f.oauth.Exchange(r.Context(), q.Get("code"), oauth2.SetAuthURLParam("code_verifier", f.verifier))
	if err != nil {
		fmt.Fprintf(w, callbackPage, "Login failed", html.EscapeString(err.Error()))
		report(tokenResult{err: err})
		return
	}
	token := tok.AccessToken
	if idt, ok := tok.Extra("id_token").(string); ok && f.provider.UseIDToken {
		token = idt
	}
	fmt.Fprintf(w, callbackPage, "Logged in to NetBird", "You can close this tab and go back to PhoneBridge.")
	report(tokenResult{token: token})
}

func beginDevice(p providerConfig) (*ssoFlow, error) {
	form := url.Values{"client_id": {p.ClientID}, "audience": {p.Audience}, "scope": {p.Scope}}
	resp, err := http.PostForm(p.DeviceEndpoint, form)
	if err != nil {
		return nil, fmt.Errorf("device authorization request: %w", err)
	}
	defer resp.Body.Close()
	var body struct {
		DeviceCode              string `json:"device_code"`
		UserCode                string `json:"user_code"`
		VerificationURI         string `json:"verification_uri"`
		VerificationURIComplete string `json:"verification_uri_complete"`
		ExpiresIn               int    `json:"expires_in"`
		Interval                int    `json:"interval"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&body); err != nil || resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("device authorization failed (HTTP %d): %v", resp.StatusCode, err)
	}
	if body.Interval <= 0 {
		body.Interval = 5
	}
	if body.ExpiresIn <= 0 {
		body.ExpiresIn = 300
	}
	verify := body.VerificationURIComplete
	if verify == "" {
		verify = body.VerificationURI
	}
	p.AuthEndpoint = verify
	f := &ssoFlow{kind: "device", provider: p, deviceCode: body.DeviceCode, state: body.UserCode,
		interval: time.Duration(body.Interval) * time.Second,
		expires:  time.Now().Add(time.Duration(body.ExpiresIn) * time.Second),
		result:   make(chan tokenResult, 1)}
	f.ctx, f.cancel = context.WithDeadline(context.Background(), f.expires)
	go f.pollDevice()
	return f, nil
}

func (f *ssoFlow) pollDevice() {
	for {
		select {
		case <-f.ctx.Done():
			return // nb_sso_wait reports timeout / cancellation
		case <-time.After(f.interval):
		}
		form := url.Values{
			"grant_type":  {"urn:ietf:params:oauth:grant-type:device_code"},
			"device_code": {f.deviceCode},
			"client_id":   {f.provider.ClientID},
		}
		resp, err := http.PostForm(f.provider.TokenEndpoint, form)
		if err != nil {
			continue
		}
		var body struct {
			AccessToken string `json:"access_token"`
			IDToken     string `json:"id_token"`
			Error       string `json:"error"`
			Description string `json:"error_description"`
		}
		_ = json.NewDecoder(resp.Body).Decode(&body)
		resp.Body.Close()
		switch body.Error {
		case "":
			token := body.AccessToken
			if f.provider.UseIDToken && body.IDToken != "" {
				token = body.IDToken
			}
			f.result <- tokenResult{token: token}
			return
		case "authorization_pending":
		case "slow_down":
			f.interval += 5 * time.Second
		default:
			f.result <- tokenResult{err: fmt.Errorf("%s: %s", body.Error, body.Description)}
			return
		}
	}
}

func (f *ssoFlow) close() {
	if f.cancel != nil {
		f.cancel()
	}
	for _, s := range f.servers {
		_ = s.Close()
	}
}

// nb_sso_wait blocks until the browser login finishes (or fails / times out /
// is cancelled) and returns the token to pass as jwt_token to nb_new.
//
//export nb_sso_wait
func nb_sso_wait(id C.int, tokenOut **C.char, errOut **C.char) C.int {
	mu.Lock()
	f, ok := flows[id]
	mu.Unlock()
	if !ok {
		return fail(errOut, errors.New("unknown login flow"))
	}
	defer func() {
		mu.Lock()
		delete(flows, id)
		mu.Unlock()
		f.close()
	}()
	select {
	case res := <-f.result:
		if res.err != nil {
			return fail(errOut, res.err)
		}
		putString(tokenOut, res.token)
		return 0
	case <-f.ctx.Done():
		if errors.Is(f.ctx.Err(), context.DeadlineExceeded) {
			return fail(errOut, fmt.Errorf("the browser login wasn't finished within %d minutes", int(ssoTimeout.Minutes())))
		}
		return fail(errOut, errors.New("login cancelled"))
	}
}

//export nb_sso_cancel
func nb_sso_cancel(id C.int) {
	mu.Lock()
	f, ok := flows[id]
	mu.Unlock()
	if ok {
		f.close()
	}
}

// ---- logout -----------------------------------------------------------------

// nb_logout tells the management server this peer logged out (best effort;
// the caller deletes the local config afterwards).
//
//export nb_logout
func nb_logout(mgmtURL, configPath *C.char, errOut **C.char) C.int {
	keyStr, err := savedPrivateKey(C.GoString(configPath))
	if err != nil {
		return fail(errOut, err)
	}
	key, err := wgtypes.ParseKey(keyStr)
	if err != nil {
		return fail(errOut, err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	client, err := mgmtClient(ctx, C.GoString(mgmtURL), key)
	if err != nil {
		return fail(errOut, err)
	}
	defer client.Close()
	if err := client.Logout(); err != nil {
		return fail(errOut, err)
	}
	return 0
}
