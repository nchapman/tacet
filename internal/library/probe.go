package library

import (
	"encoding/binary"
	"io"
	"math"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// AudioProps holds stream-level technical properties probed from audio headers.
type AudioProps struct {
	SampleRate int
	BitDepth   int
	Channels   int
	Duration   time.Duration
}

// ProbeAudio reads stream-level audio properties (sample rate, bit depth,
// channels, duration) directly from file headers without decoding audio frames.
func ProbeAudio(path string) AudioProps {
	f, err := os.Open(filepath.Clean(path)) //nolint:gosec // local library path from configured music root
	if err != nil {
		return AudioProps{}
	}
	defer func() { _ = f.Close() }()

	switch strings.ToLower(filepath.Ext(path)) {
	case ".flac":
		return probeFLAC(f)
	case ".wav":
		return probeWAV(f)
	case ".aiff", ".aif":
		return probeAIFF(f)
	case ".dsf":
		return probeDSF(f)
	default:
		return AudioProps{}
	}
}

// probeFLAC parses the mandatory 34-byte STREAMINFO metadata block.
// Spec: https://xiph.org/flac/format.html#metadata_block_streaminfo
func probeFLAC(r io.ReadSeeker) AudioProps {
	var hdr [42]byte // 4-byte "fLaC" + 4-byte block header + 34-byte STREAMINFO
	if _, err := io.ReadFull(r, hdr[:]); err != nil {
		return AudioProps{}
	}
	if string(hdr[0:4]) != "fLaC" {
		return AudioProps{}
	}
	blockType := hdr[4] & 0x7F
	blockLen := int(hdr[5])<<16 | int(hdr[6])<<8 | int(hdr[7])
	if blockType != 0 || blockLen < 34 {
		return AudioProps{}
	}

	si := hdr[8:42]
	// Bytes 10..17 of STREAMINFO pack:
	//   20 bits sampleRate | 3 bits (channels-1) | 5 bits (bps-1) | 36 bits totalSamples
	sampleRate := int(si[10])<<12 | int(si[11])<<4 | int(si[12]>>4)
	channels := int((si[12]>>1)&0x07) + 1
	bitDepth := int(((si[12]&0x01)<<4)|(si[13]>>4)) + 1
	totalSamples := (uint64(si[13]&0x0F) << 32) |
		(uint64(si[14]) << 24) |
		(uint64(si[15]) << 16) |
		(uint64(si[16]) << 8) |
		uint64(si[17])

	var dur time.Duration
	if sampleRate > 0 && totalSamples > 0 {
		dur = time.Duration(float64(totalSamples)/float64(sampleRate)*float64(time.Second) + 0.5)
	}
	return AudioProps{
		SampleRate: sampleRate,
		BitDepth:   bitDepth,
		Channels:   channels,
		Duration:   dur,
	}
}

// probeWAV parses RIFF/WAVE "fmt " and "data" chunks.
func probeWAV(r io.ReadSeeker) AudioProps {
	var riff [12]byte
	if _, err := io.ReadFull(r, riff[:]); err != nil {
		return AudioProps{}
	}
	if string(riff[0:4]) != "RIFF" || string(riff[8:12]) != "WAVE" {
		return AudioProps{}
	}

	var props AudioProps
	var byteRate uint32
	var dataSize uint32

	var chdr [8]byte
	for i := 0; i < 64; i++ {
		if _, err := io.ReadFull(r, chdr[:]); err != nil {
			break
		}
		id := string(chdr[0:4])
		size := binary.LittleEndian.Uint32(chdr[4:8])
		nextOffset := int64(size)
		if nextOffset%2 == 1 {
			nextOffset++
		}

		switch id {
		case "fmt ":
			if size >= 16 {
				var fmtBuf [16]byte
				if _, err := io.ReadFull(r, fmtBuf[:]); err != nil {
					return props
				}
				props.Channels = int(binary.LittleEndian.Uint16(fmtBuf[2:4]))
				props.SampleRate = int(binary.LittleEndian.Uint32(fmtBuf[4:8]))
				byteRate = binary.LittleEndian.Uint32(fmtBuf[8:12])
				props.BitDepth = int(binary.LittleEndian.Uint16(fmtBuf[14:16]))
				if nextOffset > 16 {
					_, _ = r.Seek(nextOffset-16, io.SeekCurrent)
				}
			} else {
				_, _ = r.Seek(nextOffset, io.SeekCurrent)
			}
		case "data":
			dataSize = size
			if props.SampleRate > 0 {
				goto done
			}
			_, _ = r.Seek(nextOffset, io.SeekCurrent)
		default:
			if _, err := r.Seek(nextOffset, io.SeekCurrent); err != nil {
				goto done
			}
		}
	}

done:
	if byteRate > 0 && dataSize > 0 {
		props.Duration = time.Duration(float64(dataSize)/float64(byteRate)*float64(time.Second) + 0.5)
	}
	return props
}

// probeAIFF parses FORM/AIFF (or AIFC) COMM chunk.
func probeAIFF(r io.ReadSeeker) AudioProps {
	var form [12]byte
	if _, err := io.ReadFull(r, form[:]); err != nil {
		return AudioProps{}
	}
	if string(form[0:4]) != "FORM" || (string(form[8:12]) != "AIFF" && string(form[8:12]) != "AIFC") {
		return AudioProps{}
	}

	var chdr [8]byte
	for i := 0; i < 64; i++ {
		if _, err := io.ReadFull(r, chdr[:]); err != nil {
			break
		}
		id := string(chdr[0:4])
		size := binary.BigEndian.Uint32(chdr[4:8])
		nextOffset := int64(size)
		if nextOffset%2 == 1 {
			nextOffset++
		}

		if id == "COMM" && size >= 18 {
			var comm [18]byte
			if _, err := io.ReadFull(r, comm[:]); err != nil {
				return AudioProps{}
			}
			channels := int(binary.BigEndian.Uint16(comm[0:2]))
			numFrames := binary.BigEndian.Uint32(comm[2:6])
			bitDepth := int(binary.BigEndian.Uint16(comm[6:8]))
			sampleRate := int(math.Round(decodeIEEE80(comm[8:18])))

			var dur time.Duration
			if sampleRate > 0 && numFrames > 0 {
				dur = time.Duration(float64(numFrames)/float64(sampleRate)*float64(time.Second) + 0.5)
			}
			return AudioProps{
				SampleRate: sampleRate,
				BitDepth:   bitDepth,
				Channels:   channels,
				Duration:   dur,
			}
		}
		if _, err := r.Seek(nextOffset, io.SeekCurrent); err != nil {
			break
		}
	}
	return AudioProps{}
}

// probeDSF parses Sony DSF ("DSD " + "fmt ") headers for DSD64/128/256.
func probeDSF(r io.ReadSeeker) AudioProps {
	var hdr [80]byte // 28-byte DSD chunk + 52-byte fmt chunk
	if _, err := io.ReadFull(r, hdr[:]); err != nil {
		return AudioProps{}
	}
	if string(hdr[0:4]) != "DSD " || string(hdr[28:32]) != "fmt " {
		return AudioProps{}
	}
	f := hdr[28:]
	channels := int(binary.LittleEndian.Uint32(f[24:28]))
	sampleRate := int(binary.LittleEndian.Uint32(f[28:32]))
	bitDepth := int(binary.LittleEndian.Uint32(f[32:36]))
	sampleCount := binary.LittleEndian.Uint64(f[36:44])

	var dur time.Duration
	if sampleRate > 0 && sampleCount > 0 {
		dur = time.Duration(float64(sampleCount)/float64(sampleRate)*float64(time.Second) + 0.5)
	}
	return AudioProps{
		SampleRate: sampleRate,
		BitDepth:   bitDepth,
		Channels:   channels,
		Duration:   dur,
	}
}

// decodeIEEE80 decodes an 80-bit big-endian IEEE 754 extended precision float
// (used for AIFF sample rates).
func decodeIEEE80(b []byte) float64 {
	if len(b) != 10 {
		return 0
	}
	exp := int(binary.BigEndian.Uint16(b[0:2]))
	sign := 1.0
	if exp&0x8000 != 0 {
		sign = -1.0
		exp &= 0x7FFF
	}
	mant := binary.BigEndian.Uint64(b[2:10])
	if exp == 0 && mant == 0 {
		return 0
	}
	if exp == 0x7FFF {
		return 0
	}
	return sign * math.Ldexp(float64(mant), exp-16383-63)
}
