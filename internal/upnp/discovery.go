package upnp

import (
	"context"
	"fmt"
	"net"
	"net/url"
	"sort"
	"strings"
	"time"

	"github.com/huin/goupnp"
	"github.com/huin/goupnp/dcps/av1"
)

// Discover scans the local network via SSDP for UPnP/DLNA MediaRenderers
// exposing AVTransport:1.
func Discover(ctx context.Context) ([]*UPnPRenderer, error) {
	maybeDevices, err := goupnp.DiscoverDevicesCtx(ctx, av1.URN_AVTransport_1)
	if err != nil {
		return nil, fmt.Errorf("SSDP discovery failed: %w", err)
	}

	byID := make(map[string]*UPnPRenderer)
	for _, d := range maybeDevices {
		if d.Err != nil || d.Root == nil || d.Location == nil {
			continue
		}
		r, err := buildRenderer(ctx, d.Root, d.Location, d.LocalAddr)
		if err != nil {
			continue
		}
		byID[r.info.ID] = r
	}

	out := make([]*UPnPRenderer, 0, len(byID))
	for _, r := range byID {
		out = append(out, r)
	}
	sort.Slice(out, func(i, j int) bool {
		return strings.ToLower(out[i].info.Name) < strings.ToLower(out[j].info.Name)
	})
	return out, nil
}

// ConnectByURL connects directly to a renderer's UPnP device description XML
// (e.g. "http://192.168.1.50:49152/description.xml" for a WiiM Ultra).
// Only private LAN HTTP/HTTPS targets are allowed to prevent SSRF to loopback,
// cloud metadata, or external hosts.
func ConnectByURL(ctx context.Context, rawURL string) (*UPnPRenderer, error) {
	loc, err := validateRendererURL(ctx, rawURL)
	if err != nil {
		return nil, err
	}
	root, err := goupnp.DeviceByURLCtx(ctx, loc)
	if err != nil {
		return nil, fmt.Errorf("fetch device description %s: %w", loc.String(), err)
	}
	return buildRenderer(ctx, root, loc, nil)
}

func validateRendererURL(ctx context.Context, rawURL string) (*url.URL, error) {
	loc, err := url.Parse(strings.TrimSpace(rawURL))
	if err != nil || loc.Host == "" {
		return nil, fmt.Errorf("invalid renderer description URL %q", rawURL)
	}
	if loc.Scheme != "http" && loc.Scheme != "https" {
		return nil, fmt.Errorf("renderer URL must use http or https scheme, got %q", loc.Scheme)
	}
	host := loc.Hostname()
	if host == "" {
		return nil, fmt.Errorf("renderer URL missing host: %q", rawURL)
	}

	var ips []net.IP
	if parsed := net.ParseIP(host); parsed != nil {
		ips = []net.IP{parsed}
	} else {
		addrs, err := net.DefaultResolver.LookupIPAddr(ctx, host)
		if err != nil || len(addrs) == 0 {
			return nil, fmt.Errorf("could not resolve renderer host %q", host)
		}
		for _, a := range addrs {
			ips = append(ips, a.IP)
		}
	}

	for _, ip := range ips {
		if !ip.IsPrivate() || ip.IsLoopback() || ip.IsLinkLocalUnicast() || ip.IsLinkLocalMulticast() || ip.IsUnspecified() {
			return nil, fmt.Errorf("renderer host %q resolves to non-LAN IP %s", host, ip.String())
		}
	}

	// Pin the validated private IP into the URL to prevent DNS-rebinding TOCTOU
	// between validation and fetch.
	pinned := *loc
	if port := loc.Port(); port != "" {
		pinned.Host = net.JoinHostPort(ips[0].String(), port)
	} else {
		pinned.Host = ips[0].String()
	}
	return &pinned, nil
}

func buildRenderer(ctx context.Context, root *goupnp.RootDevice, loc *url.URL, localAddr net.IP) (*UPnPRenderer, error) {
	avClients, err := av1.NewAVTransport1ClientsFromRootDevice(root, loc)
	if err != nil || len(avClients) == 0 {
		return nil, fmt.Errorf("device %q has no AVTransport:1 service", root.Device.FriendlyName)
	}
	pinnedAV, err := validateRendererURL(ctx, avClients[0].Service.ControlURL.URL.String())
	if err != nil {
		return nil, fmt.Errorf("unsafe AVTransport control URL: %w", err)
	}
	avClients[0].Service.ControlURL.URL = *pinnedAV
	avClients[0].SOAPClient.EndpointURL = *pinnedAV
	avClients[0].SOAPClient.HTTPClient = newSOAPHTTPClient()

	// Inspect AVTransport SCPD to learn optional action support up front.
	// Default to true if SCPD cannot be fetched so we never falsely disable
	// gapless/seek due to a transient fetch error.
	supportsSetNext := true
	supportsSeek := true
	if avClients[0].Service.SCPDURL.Ok {
		if pinnedSCPD, err := validateRendererURL(ctx, avClients[0].Service.SCPDURL.URL.String()); err == nil {
			avClients[0].Service.SCPDURL.URL = *pinnedSCPD
			sctx, cancel := context.WithTimeout(ctx, 2*time.Second)
			scpdDoc, scpdErr := avClients[0].Service.RequestSCPDCtx(sctx)
			cancel()
			if scpdErr == nil && scpdDoc != nil && len(scpdDoc.Actions) > 0 {
				supportsSetNext = scpdDoc.GetAction("SetNextAVTransportURI") != nil
				supportsSeek = scpdDoc.GetAction("Seek") != nil
			}
		}
	}

	var avEventURL string
	if avClients[0].Service.EventSubURL.Ok {
		if pinned, err := validateRendererURL(ctx, avClients[0].Service.EventSubURL.URL.String()); err == nil {
			avEventURL = pinned.String()
		}
	}

	var rcClient *av1.RenderingControl1
	var rcEventURL string
	if rcClients, err := av1.NewRenderingControl1ClientsFromRootDevice(root, loc); err == nil && len(rcClients) > 0 {
		if pinnedRC, err := validateRendererURL(ctx, rcClients[0].Service.ControlURL.URL.String()); err == nil {
			rcClients[0].Service.ControlURL.URL = *pinnedRC
			rcClients[0].SOAPClient.EndpointURL = *pinnedRC
			rcClients[0].SOAPClient.HTTPClient = newSOAPHTTPClient()
			rcClient = rcClients[0]
			if rcClient.Service.EventSubURL.Ok {
				if pinned, err := validateRendererURL(ctx, rcClient.Service.EventSubURL.URL.String()); err == nil {
					rcEventURL = pinned.String()
				}
			}
		}
	}

	var cmClient *av1.ConnectionManager1
	var sinkProtocols []string
	if cmClients, err := av1.NewConnectionManager1ClientsFromRootDevice(root, loc); err == nil && len(cmClients) > 0 {
		if pinnedCM, err := validateRendererURL(ctx, cmClients[0].Service.ControlURL.URL.String()); err == nil {
			cmClients[0].Service.ControlURL.URL = *pinnedCM
			cmClients[0].SOAPClient.EndpointURL = *pinnedCM
			cmClients[0].SOAPClient.HTTPClient = newSOAPHTTPClient()
			cmClient = cmClients[0]
			pctx, cancel := context.WithTimeout(ctx, 2*time.Second)
			_, sink, err := cmClient.GetProtocolInfoCtx(pctx)
			cancel()
			if err == nil && sink != "" {
				for _, p := range strings.Split(sink, ",") {
					p = strings.TrimSpace(p)
					if p != "" {
						sinkProtocols = append(sinkProtocols, p)
					}
				}
			}
		}
	}

	id := strings.TrimSpace(root.Device.UDN)
	if id == "" {
		id = loc.String()
	}
	name := strings.TrimSpace(root.Device.FriendlyName)
	if name == "" {
		name = loc.Hostname()
	}

	return &UPnPRenderer{
		info: RendererInfo{
			ID:              id,
			Name:            name,
			Manufacturer:    strings.TrimSpace(root.Device.Manufacturer),
			Model:           strings.TrimSpace(root.Device.ModelName),
			Location:        loc.String(),
			Host:            loc.Hostname(),
			HasVolume:       rcClient != nil,
			SupportsSetNext: supportsSetNext,
			SupportsSeek:    supportsSeek,
			SupportsEvents:  avEventURL != "",
			SinkProtocols:   sinkProtocols,
		},
		supportsSetNext: supportsSetNext,
		supportsSeek:    supportsSeek,
		localIP:         outboundIPForHost(loc, localAddr),
		avEventURL:      avEventURL,
		rcEventURL:      rcEventURL,
		av:              avClients[0],
		rc:              rcClient,
		cm:              cmClient,
	}, nil
}
