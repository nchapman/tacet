package upnp

import (
	"encoding/xml"
	"errors"
	"io"
	"strconv"
	"strings"
	"time"
)

// ParseNotifyBody parses a UPnP GENA NOTIFY XML propertyset payload for
// AVTransport or RenderingControl services.
func ParseNotifyBody(service string, r io.Reader) (DeviceEvent, error) {
	ev := DeviceEvent{Service: service}

	dec := xml.NewDecoder(r)
	dec.Strict = false

	var lastChange string
	for {
		tok, err := dec.Token()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return ev, err
		}
		start, ok := tok.(xml.StartElement)
		if !ok {
			continue
		}
		switch start.Name.Local {
		case "LastChange":
			var inner string
			if err := dec.DecodeElement(&inner, &start); err == nil {
				lastChange = strings.TrimSpace(inner)
			}
		case "TransportState":
			var val string
			if err := dec.DecodeElement(&val, &start); err == nil {
				if v := strings.TrimSpace(val); v != "" {
					ev.HasState = true
					ev.State = v
				}
			}
		case "CurrentTrackURI", "AVTransportURI":
			var val string
			if err := dec.DecodeElement(&val, &start); err == nil {
				if v := strings.TrimSpace(val); v != "" && (!ev.HasTrackURI || start.Name.Local == "CurrentTrackURI") {
					ev.HasTrackURI = true
					ev.TrackURI = v
				}
			}
		}
	}

	if lastChange != "" {
		if err := parseLastChangeXML(lastChange, &ev); err != nil {
			return ev, err
		}
	}
	return ev, nil
}

func parseLastChangeXML(raw string, ev *DeviceEvent) error {
	dec := xml.NewDecoder(strings.NewReader(raw))
	dec.Strict = false

	inInstanceZero := true
	for {
		tok, err := dec.Token()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
		start, ok := tok.(xml.StartElement)
		if !ok {
			continue
		}
		switch start.Name.Local {
		case "InstanceID":
			val := attrVal(start.Attr, "val")
			inInstanceZero = val == "" || val == "0"
		case "TransportState":
			if !inInstanceZero {
				continue
			}
			if v := strings.TrimSpace(attrVal(start.Attr, "val")); v != "" {
				ev.HasState = true
				ev.State = v
			}
		case "CurrentTrackURI":
			if !inInstanceZero {
				continue
			}
			if v := strings.TrimSpace(attrVal(start.Attr, "val")); v != "" {
				ev.HasTrackURI = true
				ev.TrackURI = v
			}
		case "AVTransportURI":
			if !inInstanceZero {
				continue
			}
			// Prefer CurrentTrackURI if both appear in the same LastChange event.
			if v := strings.TrimSpace(attrVal(start.Attr, "val")); v != "" && !ev.HasTrackURI {
				ev.HasTrackURI = true
				ev.TrackURI = v
			}
		case "CurrentTrackDuration":
			if !inInstanceZero {
				continue
			}
			if v := strings.TrimSpace(attrVal(start.Attr, "val")); v != "" {
				if d := ParseUPnPDuration(v); d > 0 {
					ev.HasDuration = true
					ev.Duration = d
				}
			}
		case "Volume":
			if !inInstanceZero {
				continue
			}
			ch := strings.TrimSpace(attrVal(start.Attr, "channel"))
			if ch != "" && !strings.EqualFold(ch, "Master") {
				continue
			}
			if v, err := strconv.Atoi(strings.TrimSpace(attrVal(start.Attr, "val"))); err == nil {
				if v < 0 {
					v = 0
				}
				if v > 100 {
					v = 100
				}
				ev.HasVolume = true
				ev.Volume = v
			}
		case "Mute":
			if !inInstanceZero {
				continue
			}
			ch := strings.TrimSpace(attrVal(start.Attr, "channel"))
			if ch != "" && !strings.EqualFold(ch, "Master") {
				continue
			}
			rawVal := strings.TrimSpace(attrVal(start.Attr, "val"))
			if rawVal != "" {
				ev.HasMute = true
				ev.Muted = rawVal == "1" || strings.EqualFold(rawVal, "true")
			}
		}
	}
	return nil
}

func attrVal(attrs []xml.Attr, name string) string {
	for _, a := range attrs {
		if strings.EqualFold(a.Name.Local, name) {
			return a.Value
		}
	}
	return ""
}

func parseGENATimeout(hdr string) time.Duration {
	hdr = strings.TrimSpace(hdr)
	lower := strings.ToLower(hdr)
	if strings.HasPrefix(lower, "second-") {
		if sec, err := strconv.Atoi(strings.TrimSpace(lower[len("second-"):])); err == nil && sec > 0 {
			if sec < 30 {
				sec = 30
			}
			if sec > 1800 {
				sec = 1800
			}
			return time.Duration(sec) * time.Second
		}
	}
	return defaultGENATimeoutSec * time.Second
}
