package mobile

import "github.com/licash-org/licash/apps/mobile/chain/node"

// The blocks this release vouches for (node.Pin), set by the files cmd/pin
// generates, mobile/pin_<network>.go: none without (README.md).
var (
	bitcoinPin *node.Pin
	signetPin  *node.Pin
)
