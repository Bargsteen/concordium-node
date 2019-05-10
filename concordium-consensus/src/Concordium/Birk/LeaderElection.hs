module Concordium.Birk.LeaderElection where

import           Data.ByteString.Builder
import qualified Data.ByteString.Lazy          as L
import           Data.ByteString

import qualified Concordium.Crypto.VRF    as VRF
import           Concordium.GlobalState.Parameters
import           Concordium.Types

electionProbability :: LotteryPower -> ElectionDifficulty -> Double
electionProbability alpha diff = 1 - (1 - diff) ** alpha

leaderElectionMessage :: LeadershipElectionNonce -> Slot -> ByteString
leaderElectionMessage nonce (Slot sl) =
  L.toStrict
    $  toLazyByteString
    $  stringUtf8 "LE"
    <> byteString nonce
    <> word64BE sl

leaderElection
  :: LeadershipElectionNonce
  -> ElectionDifficulty
  -> Slot
  -> BakerElectionPrivateKey
  -> LotteryPower
  -> IO (Maybe BlockProof)
leaderElection nonce diff slot key lotPow = do
        let msg = leaderElectionMessage nonce slot
        proof <- VRF.prove key msg
        let hsh = VRF.proofToHash proof
        return $ if VRF.hashToDouble hsh < electionProbability lotPow diff
                    then Just proof
                    else Nothing

verifyProof
  :: LeadershipElectionNonce
  -> ElectionDifficulty
  -> Slot
  -> BakerElectionVerifyKey
  -> LotteryPower
  -> BlockProof
  -> Bool
verifyProof nonce diff slot verifKey lotPow proof =
  VRF.verifyKey verifKey
    && VRF.verify verifKey (leaderElectionMessage nonce slot) proof
    && VRF.hashToDouble (VRF.proofToHash proof)
    <  electionProbability lotPow diff

electionLuck :: ElectionDifficulty -> LotteryPower -> BlockProof -> Double
electionLuck diff lotPow proof =
  1 - VRF.hashToDouble (VRF.proofToHash proof) / electionProbability lotPow diff


blockNonceMessage :: LeadershipElectionNonce -> Slot -> ByteString
blockNonceMessage nonce (Slot slot) =
  L.toStrict
    $  toLazyByteString
    $  stringUtf8 "NONCE"
    <> byteString nonce
    <> word64BE slot

computeBlockNonce
  :: LeadershipElectionNonce -> Slot -> BakerElectionPrivateKey -> IO BlockNonce
computeBlockNonce nonce slot key = do
        let msg = blockNonceMessage nonce slot
        proof <- VRF.prove key msg
        return (VRF.proofToHash proof, proof)

verifyBlockNonce
  :: LeadershipElectionNonce
  -> Slot
  -> BakerElectionVerifyKey
  -> BlockNonce
  -> Bool
verifyBlockNonce nonce slot verifKey (_hsh, prf) =
  VRF.verifyKey verifKey
    && VRF.verify verifKey (blockNonceMessage nonce slot) prf
