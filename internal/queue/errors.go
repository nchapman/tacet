package queue

import (
	"errors"
	"fmt"
)

// Domain sentinel errors returned by queue and transport operations.
var (
	ErrZoneNotFound          = errors.New("zone not found")
	ErrQueueEmpty            = errors.New("queue is empty")
	ErrEndOfQueue            = errors.New("already at end of queue")
	ErrQueueItemNotFound     = errors.New("queue item not found")
	ErrInvalidArgument       = errors.New("invalid argument")
	ErrCapabilityUnsupported = errors.New("capability unsupported by renderer")
	ErrRendererUnreachable   = errors.New("renderer communication failed")
)

func wrapRendererErr(action string, err error) error {
	if err == nil {
		return nil
	}
	return fmt.Errorf("%w: %s: %w", ErrRendererUnreachable, action, err)
}
