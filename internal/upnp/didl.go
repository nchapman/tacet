package upnp

import (
	"bytes"
	"encoding/xml"
	"fmt"
	"strconv"
	"strings"
	"time"
)

// DLNAFeaturesSeekable is the standard DLNA fourth-field protocolInfo for
// byte-range seekable, non-transcoded audio streams.
const DLNAFeaturesSeekable = "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"

// DLNAFeaturesStreaming is used when byte-range seeking is unavailable.
const DLNAFeaturesStreaming = "DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"

// MediaItem describes an audio resource to hand to a UPnP/DLNA renderer.
type MediaItem struct {
	ID          string
	URL         string
	Title       string
	Artist      string
	Album       string
	AlbumArtURL string
	TrackNumber int
	MIME        string
	Duration    time.Duration
	Size        int64
	SampleRate  int
	BitDepth    int
	Channels    int
	Seekable    bool
}

// NegotiateMIME picks the MIME type spelling a renderer actually advertises in
// ConnectionManager.GetProtocolInfo. Notably, WiiM / LinkPlay renderers
// advertise "audio/flac" (RFC 9639) and reject "audio/x-flac".
func NegotiateMIME(preferred string, sinkProtocols []string) string {
	preferred = strings.ToLower(strings.TrimSpace(preferred))
	if preferred == "" {
		preferred = "audio/flac"
	}
	if len(sinkProtocols) == 0 {
		return preferred
	}

	sinkSet := make(map[string]struct{}, len(sinkProtocols))
	for _, entry := range sinkProtocols {
		parts := strings.Split(strings.TrimSpace(entry), ":")
		if len(parts) >= 3 {
			sinkSet[strings.ToLower(strings.TrimSpace(parts[2]))] = struct{}{}
		}
	}

	if _, ok := sinkSet[preferred]; ok {
		return preferred
	}

	aliases := map[string][]string{
		"audio/flac":   {"audio/x-flac"},
		"audio/x-flac": {"audio/flac"},
		"audio/wav":    {"audio/x-wav", "audio/wave", "audio/L16"},
		"audio/x-wav":  {"audio/wav", "audio/wave"},
		"audio/aiff":   {"audio/x-aiff"},
		"audio/x-aiff": {"audio/aiff"},
		"audio/mp4":    {"audio/x-m4a", "audio/m4a", "audio/alac"},
		"audio/x-dsf":  {"audio/dsf", "audio/x-dsd", "audio/dsd"},
		"audio/dsf":    {"audio/x-dsf", "audio/x-dsd", "audio/dsd"},
		"audio/mpeg":   {"audio/mp3"},
	}

	for _, alt := range aliases[preferred] {
		if _, ok := sinkSet[alt]; ok {
			return alt
		}
	}
	return preferred
}

// BuildDIDL builds raw (unescaped) DIDL-Lite XML for AVTransport URI metadata.
// huin/goupnp XML-escapes action arguments when embedding them in SOAP XML.
func BuildDIDL(m MediaItem, sinkProtocols []string) string {
	mime := NegotiateMIME(m.MIME, sinkProtocols)
	features := DLNAFeaturesStreaming
	if m.Seekable {
		features = DLNAFeaturesSeekable
	}
	protocolInfo := fmt.Sprintf("http-get:*:%s:%s", mime, features)

	itemID := m.ID
	if itemID == "" {
		itemID = "0"
	}
	title := m.Title
	if title == "" {
		title = "Unknown Track"
	}

	var b strings.Builder
	b.WriteString(`<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" `)
	b.WriteString(`xmlns:dc="http://purl.org/dc/elements/1.1/" `)
	b.WriteString(`xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">`)
	b.WriteString(`<item id="`)
	b.WriteString(escapeXML(itemID))
	b.WriteString(`" parentID="0" restricted="1">`)

	b.WriteString(`<dc:title>`)
	b.WriteString(escapeXML(title))
	b.WriteString(`</dc:title>`)
	b.WriteString(`<upnp:class>object.item.audioItem.musicTrack</upnp:class>`)

	if m.Artist != "" {
		escaped := escapeXML(m.Artist)
		b.WriteString(`<dc:creator>`)
		b.WriteString(escaped)
		b.WriteString(`</dc:creator>`)
		b.WriteString(`<upnp:artist>`)
		b.WriteString(escaped)
		b.WriteString(`</upnp:artist>`)
	}
	if m.Album != "" {
		b.WriteString(`<upnp:album>`)
		b.WriteString(escapeXML(m.Album))
		b.WriteString(`</upnp:album>`)
	}
	if m.TrackNumber > 0 {
		b.WriteString(`<upnp:originalTrackNumber>`)
		b.WriteString(strconv.Itoa(m.TrackNumber))
		b.WriteString(`</upnp:originalTrackNumber>`)
	}
	if m.AlbumArtURL != "" {
		b.WriteString(`<upnp:albumArtURI>`)
		b.WriteString(escapeXML(m.AlbumArtURL))
		b.WriteString(`</upnp:albumArtURI>`)
	}

	b.WriteString(`<res protocolInfo="`)
	b.WriteString(escapeXML(protocolInfo))
	b.WriteString(`"`)
	if m.Duration > 0 {
		b.WriteString(` duration="`)
		b.WriteString(FormatUPnPDuration(m.Duration))
		b.WriteString(`"`)
	}
	if m.Size > 0 {
		b.WriteString(` size="`)
		b.WriteString(strconv.FormatInt(m.Size, 10))
		b.WriteString(`"`)
	}
	if m.SampleRate > 0 {
		b.WriteString(` sampleFrequency="`)
		b.WriteString(strconv.Itoa(m.SampleRate))
		b.WriteString(`"`)
	}
	if m.BitDepth > 0 {
		b.WriteString(` bitsPerSample="`)
		b.WriteString(strconv.Itoa(m.BitDepth))
		b.WriteString(`"`)
	}
	if m.Channels > 0 {
		b.WriteString(` nrAudioChannels="`)
		b.WriteString(strconv.Itoa(m.Channels))
		b.WriteString(`"`)
	}
	b.WriteString(`>`)
	b.WriteString(escapeXML(m.URL))
	b.WriteString(`</res>`)

	b.WriteString(`</item></DIDL-Lite>`)
	return b.String()
}

// FormatUPnPDuration formats a duration as H:MM:SS for UPnP AVTransport Seek/DIDL.
func FormatUPnPDuration(d time.Duration) string {
	if d < 0 {
		d = 0
	}
	totalSecs := int(d.Round(time.Second) / time.Second)
	h := totalSecs / 3600
	m := (totalSecs % 3600) / 60
	s := totalSecs % 60
	return fmt.Sprintf("%d:%02d:%02d", h, m, s)
}

// ParseUPnPDuration parses UPnP duration strings like "0:03:45" or "00:03:45.123".
func ParseUPnPDuration(raw string) time.Duration {
	raw = strings.TrimSpace(raw)
	if raw == "" || raw == "NOT_IMPLEMENTED" || raw == "UNKNOWN" {
		return 0
	}
	parts := strings.Split(raw, ":")
	if len(parts) != 3 {
		return 0
	}
	h, err1 := strconv.Atoi(parts[0])
	m, err2 := strconv.Atoi(parts[1])
	secStr := parts[2]
	var ms int
	if dot := strings.IndexByte(secStr, '.'); dot >= 0 {
		frac := secStr[dot+1:]
		secStr = secStr[:dot]
		if len(frac) == 0 {
			return 0
		}
		if len(frac) > 3 {
			frac = frac[:3]
		}
		for len(frac) < 3 {
			frac += "0"
		}
		var fracErr error
		ms, fracErr = strconv.Atoi(frac)
		if fracErr != nil || ms < 0 {
			return 0
		}
	}
	s, err3 := strconv.Atoi(secStr)
	if err1 != nil || err2 != nil || err3 != nil || h < 0 || h > 999 || m < 0 || m > 59 || s < 0 || s > 59 {
		return 0
	}
	return time.Duration(h)*time.Hour +
		time.Duration(m)*time.Minute +
		time.Duration(s)*time.Second +
		time.Duration(ms)*time.Millisecond
}

func escapeXML(s string) string {
	var buf bytes.Buffer
	_ = xml.EscapeText(&buf, []byte(s))
	return buf.String()
}
