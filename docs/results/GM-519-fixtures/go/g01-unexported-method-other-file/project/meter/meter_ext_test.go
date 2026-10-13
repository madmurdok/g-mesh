package meter_test

import (
	"testing"

	"example.com/gfix/meter"
)

func TestShow(t *testing.T) {
	_ = meter.Show(&meter.Meter{})
}
