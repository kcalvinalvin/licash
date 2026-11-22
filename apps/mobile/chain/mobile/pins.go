package mobile

import "github.com/licash-org/licash/apps/mobile/chain/node"

// The blocks this release vouches for (node.Pin), set by the files cmd/pin
// generates, mobile/pin_<network>.go: none without (README.md).
var (
	bitcoinPin   *node.Pin
	signetPin    *node.Pin
	mutinynetPin *node.Pin
)

// PinHeight is the height of the block this release vouches for on network
// ("bitcoin", "signet" or "mutinynet"), 0 for one it pins none of: a block
// already mined, before which a wallet made now holds nothing.
func PinHeight(network string) int32 {
	nw, err := networkOf(network)
	if err != nil || nw.pin == nil {
		return 0
	}
	return int32(nw.pin.Height)
}
