{-# LANGUAGE RecordWildCards, OverloadedStrings, ScopedTypeVariables #-}
{-# OPTIONS_GHC -Wno-deprecations #-}
module Concordium.GlobalState.DummyData where

import qualified Data.HashMap.Strict as HM
import qualified Concordium.Crypto.BlockSignature as Sig
import qualified Concordium.Crypto.VRF as VRF
import qualified Concordium.Crypto.SHA256 as Hash
import qualified Concordium.Crypto.BlsSignature as Bls
import Concordium.GlobalState.Parameters
import Concordium.GlobalState.Bakers
import Concordium.GlobalState.SeedState
import Concordium.GlobalState.IdentityProviders
import Concordium.Types
import Test.Hspec
import System.Random
import qualified Data.ByteString.Lazy.Char8 as BSL
import System.IO.Unsafe
import qualified Concordium.GlobalState.Basic.BlockState as Basic
import Concordium.Crypto.DummyData
import Concordium.Types.DummyData

{-# WARNING basicGenesisState "Do not use in production" #-}
basicGenesisState :: GenesisData -> Basic.BlockState
basicGenesisState genData = Basic.initialState
                       (genesisBirkParameters genData)
                       (genesisCryptographicParameters genData)
                       (genesisAccounts genData ++ genesisSpecialBetaAccounts genData)
                       (genesisIdentityProviders genData)
                       (genesisMintPerSlot genData)

-- kp :: Int -> Sig.KeyPair
-- kp n = fst (Sig.randomKeyPair (mkStdGen n))

-- proofKP :: Int -> VRF.KeyPair
-- proofKP n = fst (VRF.randomKeyPair (mkStdGen n))

{-# WARNING dummyCryptographicParameters "Do not use in production" #-}
dummyCryptographicParameters :: CryptographicParameters
dummyCryptographicParameters =
  case unsafePerformIO (readCryptographicParameters <$> BSL.readFile "testdata/global.json") of
    Nothing -> error "Could not read cryptographic parameters."
    Just params -> params

{-# NOINLINE dummyIdentityProviders #-}
{-# WARNING dummyIdentityProviders "Do not use in production." #-}
dummyIdentityProviders :: IdentityProviders
dummyIdentityProviders =
  case unsafePerformIO (eitherReadIdentityProviders <$> BSL.readFile "testdata/identity_providers.json") of
    Left err -> error $ "Could not load identity provider test data: " ++ err
    Right ips -> IdentityProviders (HM.fromList (map (\r -> (ipIdentity r, r)) ips))

{-# WARNING makeFakeBakers "Do not use in production" #-}
makeFakeBakers :: Word -> [(BakerInfo, Account)]
makeFakeBakers nBakers = take (fromIntegral nBakers) $ mbs (mkStdGen 17) 0
    where
        mbs gen bid = (BakerInfo epk spk blspk stake accAddress, account):mbs gen''' (bid+1)
            where
                ((VRF.KeyPair _ epk), gen') = VRF.randomKeyPair gen
                (sk, gen'') = randomBlockKeyPair gen'
                spk = Sig.verifyKey sk
                (blssk, gen''') = randomBlsSecretKey gen''
                blspk = Bls.derivePublicKey blssk
                accAddress = _accountAddress account
                stake = _accountAmount account
                account = makeFakeBakerAccount bid

-- |Make a baker deterministically from a given seed and with the given reward account.
-- Uses 'bakerElectionKey' and 'bakerSignKey' with the given seed to generate the keys.
-- The baker has 0 lottery power.
-- mkBaker :: Int -> AccountAddress -> (BakerInfo
{-# WARNING mkFullBaker "Do not use in production." #-}
mkFullBaker :: Int -> AccountAddress -> (BakerInfo, VRF.SecretKey, Sig.SignKey, Bls.SecretKey)
mkFullBaker seed acc = (BakerInfo {
  _bakerElectionVerifyKey = VRF.publicKey electionKey,
  _bakerSignatureVerifyKey = Sig.verifyKey sk,
  _bakerAggregationVerifyKey = Bls.derivePublicKey blssk,
  _bakerStake = 0,
  _bakerAccount = acc
  }, VRF.privateKey electionKey, Sig.signKey sk, blssk)
  where electionKey = bakerElectionKey seed
        sk = bakerSignKey seed
        blssk = bakerAggregationKey seed

{-# WARNING makeTestingGenesisData "Do not use in production" #-}
makeTestingGenesisData ::
    Timestamp -- ^Genesis time
    -> Word  -- ^Initial number of bakers.
    -> Duration  -- ^Slot duration in seconds.
    -> ElectionDifficulty  -- ^Initial election difficulty.
    -> BlockHeight -- ^Minimum finalization interval - 1
    -> CryptographicParameters -- ^Initial cryptographic parameters.
    -> [IpInfo]   -- ^List of initial identity providers.
    -> [Account]  -- ^List of starting genesis special accounts (in addition to baker accounts).
    -> GenesisData
makeTestingGenesisData genesisTime nBakers genesisSlotDuration elecDiff finMinSkip genesisCryptographicParameters genesisIdentityProviders genesisSpecialBetaAccounts
    = GenesisData{..}
    where
        genesisMintPerSlot = 10 -- default value, OK for testing.
        genesisBakers = fst (bakersFromList bakers)
        genesisBirkParameters =
            BirkParameters elecDiff -- voting power
                          genesisBakers
                          genesisBakers
                          genesisBakers
                          (genesisSeedState (Hash.hash "LeadershipElectionNonce") 10) -- todo hardcoded epoch length (and initial seed)
        genesisFinalizationParameters = FinalizationParameters [VoterInfo vvk vrfk 1 vblsk | (BakerInfo vrfk vvk vblsk _ _) <- bakers] finMinSkip
        (bakers, genesisAccounts) = unzip (makeFakeBakers nBakers)

{-# WARNING emptyBirkParameters "Do not use in production." #-}
emptyBirkParameters :: BirkParameters
emptyBirkParameters = BirkParameters {
  _birkElectionDifficulty = 0.5,
  _birkCurrentBakers = emptyBakers,
  _birkPrevEpochBakers = emptyBakers,
  _birkLotteryBakers = emptyBakers,
  _birkSeedState = genesisSeedState (Hash.hash "NONCE") 360
  }
