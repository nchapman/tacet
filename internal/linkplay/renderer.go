package linkplay

import (
	"bytes"
	"context"
	"encoding/xml"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"time"

	"tacet/internal/upnp"
)

const (
	playQueueNS       = "urn:schemas-wiimu-com:service:PlayQueue:1"
	tacetListName     = "tacet"
	currentQueue      = "CurrentQueue"
	linkplayBatchSize = 50
)

// Renderer wraps a Linkplay UPnP Renderer (WiiM, AudioPro, Arylic, etc.) and
// implements upnp.QueueController using Linkplay's native PlayQueue:1 SOAP
// service so hardware Next/Previous buttons, on-device screens, and device-
// managed queue transitions work natively.
type Renderer struct {
	*upnp.Renderer
	playQueueURL string
	httpClient   *http.Client
	fallbackMu   sync.Mutex
}

// Wrap promotes r to a Linkplay *Renderer when the UPnP device advertises
// urn:schemas-wiimu-com:service:PlayQueue:1; otherwise it returns r unchanged.
func Wrap(r *upnp.Renderer) upnp.Controller {
	if r == nil || r.PlayQueueControlURL() == "" {
		return r
	}
	return NewRenderer(r, r.PlayQueueControlURL())
}

// NewRenderer constructs a Linkplay Renderer for an explicit PlayQueue control URL.
func NewRenderer(base *upnp.Renderer, playQueueURL string) *Renderer {
	client := upnp.NewSOAPHTTPClient(8 * time.Second)
	return &Renderer{
		Renderer:     base,
		playQueueURL: playQueueURL,
		httpClient:   &client,
	}
}

// Info returns the renderer descriptor with SupportsQueue enabled.
func (r *Renderer) Info() upnp.RendererInfo {
	if r.Renderer == nil {
		return upnp.RendererInfo{SupportsQueue: true}
	}
	info := r.Renderer.Info()
	info.SupportsQueue = true
	return info
}

// ReplaceQueue replaces the Linkplay device queue with items and starts
// playback at 0-based startIndex.
func (r *Renderer) ReplaceQueue(ctx context.Context, items []upnp.MediaItem, startIndex int) error {
	if len(items) == 0 {
		return fmt.Errorf("linkplay ReplaceQueue: empty items")
	}
	if startIndex < 0 || startIndex >= len(items) {
		startIndex = 0
	}
	firstEnd := len(items)
	if firstEnd > linkplayBatchSize {
		firstEnd = linkplayBatchSize
	}
	qctx := buildQueueContextXML(tacetListName, items[:firstEnd], r.sinkProtocols())
	body := fmt.Sprintf("<QueueContext>%s</QueueContext>", xmlEscape(qctx))
	if _, err := r.soapCall(ctx, "CreateQueue", body); err != nil {
		return fmt.Errorf("linkplay CreateQueue: %w", err)
	}
	for start := firstEnd; start < len(items); start += linkplayBatchSize {
		end := start + linkplayBatchSize
		if end > len(items) {
			end = len(items)
		}
		if err := r.AppendQueue(ctx, items[start:end]); err != nil {
			return err
		}
	}
	// Linkplay PlayQueue indices are 1-based.
	playBody := fmt.Sprintf("<QueueName>%s</QueueName><Index>%d</Index>", xmlEscape(tacetListName), startIndex+1)
	if _, err := r.soapCall(ctx, "PlayQueueWithIndex", playBody); err != nil {
		return fmt.Errorf("linkplay PlayQueueWithIndex: %w", err)
	}
	return nil
}

// AppendQueue appends items to the active Linkplay CurrentQueue.
func (r *Renderer) AppendQueue(ctx context.Context, items []upnp.MediaItem) error {
	if len(items) == 0 {
		return nil
	}
	qctx := buildQueueContextXML(currentQueue, items, r.sinkProtocols())
	body := fmt.Sprintf("<QueueContext>%s</QueueContext>", xmlEscape(qctx))
	if _, err := r.soapCall(ctx, "AppendTracksInQueue", body); err != nil {
		return fmt.Errorf("linkplay AppendTracksInQueue: %w", err)
	}
	return nil
}

// RemoveQueueRange removes 0-based inclusive range [start..end] from CurrentQueue.
func (r *Renderer) RemoveQueueRange(ctx context.Context, start, end int) error {
	if start < 0 || end < start {
		return nil
	}
	body := fmt.Sprintf(
		"<QueueName>%s</QueueName><RangStart>%d</RangStart><RangEnd>%d</RangEnd><Action>0</Action>",
		currentQueue, start+1, end+1,
	)
	if _, err := r.soapCall(ctx, "RemoveTracksInQueue", body); err != nil {
		return fmt.Errorf("linkplay RemoveTracksInQueue: %w", err)
	}
	return nil
}

// MoveQueueItem moves the 0-based item at from to 0-based index to in CurrentQueue.
func (r *Renderer) MoveQueueItem(ctx context.Context, from, to int) error {
	if from < 0 || to < 0 || from == to {
		return nil
	}
	body := fmt.Sprintf(
		"<QueueName>%s</QueueName><IndexList>%d</IndexList><ToIndex>%d</ToIndex>",
		currentQueue, from+1, to+1,
	)
	if _, err := r.soapCall(ctx, "MoveTracksInQueue", body); err != nil {
		return fmt.Errorf("linkplay MoveTracksInQueue: %w", err)
	}
	return nil
}

// PlayIndex jumps playback to 0-based index in CurrentQueue.
func (r *Renderer) PlayIndex(ctx context.Context, index int) error {
	if index < 0 {
		index = 0
	}
	body := fmt.Sprintf("<QueueName>%s</QueueName><Index>%d</Index>", currentQueue, index+1)
	if _, err := r.soapCall(ctx, "PlayQueueWithIndex", body); err != nil {
		return fmt.Errorf("linkplay PlayQueueWithIndex: %w", err)
	}
	return nil
}

// SetLoopMode maps repeat ("off"|"all"|"one") to Linkplay's LoopMode integer.
// Linkplay loop modes: 0=repeat all, 1=repeat one, 4=no repeat (normal).
func (r *Renderer) SetLoopMode(ctx context.Context, repeat string, _ bool) error {
	mode := 4
	switch strings.ToLower(strings.TrimSpace(repeat)) {
	case "all":
		mode = 0
	case "one":
		mode = 1
	}
	body := fmt.Sprintf("<LoopMode>%d</LoopMode>", mode)
	if _, err := r.soapCall(ctx, "SetQueueLoopMode", body); err != nil {
		return fmt.Errorf("linkplay SetQueueLoopMode: %w", err)
	}
	return nil
}

// Status augments standard UPnP TransportStatus with the device's current
// 0-based PlayQueue index so hardware Next/Previous presses sync back to tacet.
func (r *Renderer) Status(ctx context.Context) (upnp.TransportStatus, error) {
	st, err := r.Renderer.Status(ctx)
	if err != nil {
		return st, err
	}
	if st.State == "PLAYING" || st.State == "PAUSED_PLAYBACK" || st.State == "TRANSITIONING" {
		if idx, ok := r.getQueueIndex(ctx); ok {
			st.QueueIndex = idx
			st.HasQueueIndex = true
		}
	}
	return st, nil
}

func (r *Renderer) getQueueIndex(ctx context.Context) (int, bool) {
	raw, err := r.soapCall(ctx, "GetQueueIndex", "<QueueName>"+currentQueue+"</QueueName>")
	if err != nil {
		return 0, false
	}
	return parseGetQueueIndex(raw)
}

func (r *Renderer) soapCall(ctx context.Context, action, innerBody string) (string, error) {
	env := fmt.Sprintf(
		`<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:%s xmlns:u="%s">%s</u:%s></s:Body></s:Envelope>`,
		action, playQueueNS, innerBody, action,
	)
	var out string
	doReq := func() error {
		req, err := http.NewRequestWithContext(ctx, http.MethodPost, r.playQueueURL, strings.NewReader(env))
		if err != nil {
			return err
		}
		req.Header.Set("Content-Type", `text/xml; charset="utf-8"`)
		req.Header.Set("SOAPACTION", fmt.Sprintf(`"%s#%s"`, playQueueNS, action))

		resp, err := r.httpClient.Do(req)
		if err != nil {
			return err
		}
		defer func() { _ = resp.Body.Close() }()

		data, err := io.ReadAll(io.LimitReader(resp.Body, 256*1024))
		if err != nil {
			return err
		}
		if resp.StatusCode < 200 || resp.StatusCode >= 300 {
			return fmt.Errorf("SOAP %s HTTP %d: %s", action, resp.StatusCode, strings.TrimSpace(string(data)))
		}
		out = string(data)
		return nil
	}
	var err error
	if r.Renderer != nil {
		err = r.DoSOAP(ctx, doReq)
	} else {
		r.fallbackMu.Lock()
		err = doReq()
		r.fallbackMu.Unlock()
	}
	if err != nil {
		return "", err
	}
	return out, nil
}

func (r *Renderer) sinkProtocols() []string {
	if r.Renderer == nil {
		return nil
	}
	return r.Renderer.Info().SinkProtocols
}

func buildQueueContextXML(listName string, items []upnp.MediaItem, sinkProtocols []string) string {
	var b strings.Builder
	b.WriteString(`<?xml version="1.0"?>`)
	b.WriteString("<PlayList>")
	b.WriteString("<ListName>")
	b.WriteString(xmlEscape(listName))
	b.WriteString("</ListName>")
	b.WriteString("<ListInfo>")
	b.WriteString("<Radio>0</Radio>")
	b.WriteString("<SourceName>Local</SourceName>")
	b.WriteString("<PicUrl>")
	if len(items) > 0 && items[0].AlbumArtURL != "" {
		b.WriteString(xmlEscape(items[0].AlbumArtURL))
	}
	b.WriteString("</PicUrl>")
	b.WriteString("<TrackNumber>")
	b.WriteString(strconv.Itoa(len(items)))
	b.WriteString("</TrackNumber>")
	b.WriteString("<SearchUrl></SearchUrl>")
	b.WriteString("<Quality>0</Quality>")
	b.WriteString("</ListInfo>")
	b.WriteString("<Tracks>")
	for i, item := range items {
		tag := "Track" + strconv.Itoa(i+1)
		b.WriteString("<" + tag + ">")
		b.WriteString("<URL>")
		b.WriteString(xmlEscape(item.URL))
		b.WriteString("</URL>")
		b.WriteString("<Source>OnlineMusic</Source>")
		b.WriteString("<Key></Key>")
		b.WriteString("<Id>")
		b.WriteString(xmlEscape(item.ID))
		b.WriteString("</Id>")
		b.WriteString("<Metadata>")
		b.WriteString(xmlEscape(upnp.BuildDIDL(item, sinkProtocols)))
		b.WriteString("</Metadata>")
		b.WriteString("</" + tag + ">")
	}
	b.WriteString("</Tracks>")
	b.WriteString("</PlayList>")
	return b.String()
}

func parseGetQueueIndex(soapXML string) (int, bool) {
	dec := xml.NewDecoder(strings.NewReader(soapXML))
	for {
		tok, err := dec.Token()
		if err != nil {
			return 0, false
		}
		if se, ok := tok.(xml.StartElement); ok && strings.EqualFold(se.Name.Local, "CurrentIndex") {
			var text string
			if err := dec.DecodeElement(&text, &se); err != nil {
				return 0, false
			}
			n, err := strconv.Atoi(strings.TrimSpace(text))
			if err != nil || n <= 0 {
				return 0, false
			}
			// Convert Linkplay's 1-based CurrentIndex to 0-based queue index.
			return n - 1, true
		}
	}
}

func xmlEscape(s string) string {
	var buf bytes.Buffer
	_ = xml.EscapeText(&buf, []byte(s))
	return buf.String()
}
