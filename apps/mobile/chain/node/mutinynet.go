package node

import (
	"encoding/hex"
	"time"

	"github.com/btcsuite/btcd/chaincfg/v2"
)

const (
	// mutinynetChallenge is Mutinynet's signet challenge, as published at
	// https://blog.mutinywallet.com/mutinynet/.
	mutinynetChallenge = "512102f7561d208dd9ae99bf497273e16f389bdbd6c4742ddb8e6b216e64fa2928ad8f51ae"

	// MutinynetPeer is the node Mutinynet publishes: it has no DNS seed.
	MutinynetPeer = "45.79.52.207:38333"
)

// Mutinynet is the parameters of Mutinynet, a signet of 30-second blocks.
func Mutinynet() chaincfg.Params {
	challenge, err := hex.DecodeString(mutinynetChallenge)
	if err != nil {
		panic(err)
	}
	params := chaincfg.CustomSignetParams(challenge, nil)
	params.Name = "mutinynet"
	params.TargetTimePerBlock = 30 * time.Second
	return params
}
