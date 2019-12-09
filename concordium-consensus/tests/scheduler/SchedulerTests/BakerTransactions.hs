{-# LANGUAGE RecordWildCards #-}
{-# LANGUAGE OverloadedStrings #-}
{-# LANGUAGE LambdaCase #-}
{-# OPTIONS_GHC -Wall #-}
module SchedulerTests.BakerTransactions where

import Test.Hspec

import qualified Data.Map as Map
import qualified Data.HashSet as Set
import qualified Concordium.Scheduler.Types as Types
import qualified Concordium.Scheduler.EnvironmentImplementation as Types
import qualified Acorn.Utils.Init as Init
import Concordium.Scheduler.Runner
import qualified Acorn.Parser.Runner as PR
import qualified Concordium.Scheduler as Sch

import Concordium.GlobalState.Bakers
import Concordium.GlobalState.Basic.BlockState.Account as Acc
import Concordium.GlobalState.Modules as Mod
import Concordium.GlobalState.Basic.BlockState
import Concordium.GlobalState.Basic.BlockState.Invariants
import qualified Concordium.GlobalState.Rewards as Rew

import qualified Concordium.Crypto.BlockSignature as BlockSig
import qualified Concordium.Crypto.VRF as VRF
import qualified Concordium.Crypto.BlsSignature as Bls

import qualified Acorn.Core as Core

import Lens.Micro.Platform

import SchedulerTests.DummyData

shouldReturnP :: Show a => IO a -> (a -> Bool) -> IO ()
shouldReturnP action f = action >>= (`shouldSatisfy` f)

initialBlockState :: BlockState
initialBlockState =
  emptyBlockState emptyBirkParameters dummyCryptographicParameters &
    (blockAccounts .~ Acc.putAccountWithRegIds (mkAccount alesVK 100000)
                      (Acc.putAccountWithRegIds (mkAccount thomasVK 100000) Acc.emptyAccounts)) .
    (blockBank . Rew.totalGTU .~ 200000) .
    (blockModules .~ (let (_, _, gs) = Init.baseState in Mod.fromModuleList (Init.moduleList gs)))

baker0 :: (BakerInfo, VRF.SecretKey, BlockSig.SignKey, Bls.SecretKey)
baker0 = mkBaker 0 alesAccount

baker1 :: (BakerInfo, VRF.SecretKey, BlockSig.SignKey, Bls.SecretKey)
baker1 = mkBaker 1 alesAccount

baker2 :: (BakerInfo, VRF.SecretKey, BlockSig.SignKey, Bls.SecretKey)
baker2 = mkBaker 2 thomasAccount

transactionsInput :: [TransactionJSON]
transactionsInput =
    [TJSON { payload = AddBaker (baker0 ^. _1 . bakerElectionVerifyKey)
                                (baker0 ^. _2)
                                (baker0 ^. _1 . bakerSignatureVerifyKey)
                                (baker0 ^. _1 . bakerAggregationVerifyKey)
                                (baker0 ^. _3)
                                alesKP
           , metadata = makeHeader alesKP 1 10000
           , keypair = alesKP
           },
     TJSON { payload = AddBaker (baker1 ^. _1 . bakerElectionVerifyKey)
                                (baker1 ^. _2)
                                (baker1 ^. _1 . bakerSignatureVerifyKey)
                                (baker1 ^. _1 . bakerAggregationVerifyKey)
                                (baker1 ^. _3)
                                alesKP
           , metadata = makeHeader alesKP 2 10000
           , keypair = alesKP
           },
     TJSON { payload = AddBaker (baker2 ^. _1 . bakerElectionVerifyKey)
                                (baker2 ^. _2)
                                (baker2 ^. _1 . bakerSignatureVerifyKey)
                                (baker2 ^. _1 . bakerAggregationVerifyKey)
                                (baker2 ^. _3)
                                thomasKP
           , metadata = makeHeader alesKP 3 10000
           , keypair = alesKP
           },
     TJSON { payload = RemoveBaker 1 "<dummy proof>"
           , metadata = makeHeader alesKP 4 10000
           , keypair = alesKP
           },
     TJSON { payload = UpdateBakerAccount 2 alesKP
           , metadata = makeHeader thomasKP 1 10000
           , keypair = thomasKP
           -- baker 2's account is Thomas account, so only it can update it
           },
     TJSON { payload = UpdateBakerSignKey 0 (BlockSig.verifyKey (bakerSignKey 3)) (BlockSig.signKey (bakerSignKey 3))
           , metadata = makeHeader alesKP 5 10000
           , keypair = alesKP
           -- baker 0's account is Thomas account, so only it can update it
           }
    ]

runWithIntermediateStates :: PR.Context Core.UA IO ([([(Types.BareTransaction, Types.ValidResult)],
                                                     [(Types.BareTransaction, Types.FailureKind)],
                                                     Types.BirkParameters)], BlockState)
runWithIntermediateStates = do
  txs <- processTransactions transactionsInput
  let (res, state) = foldl (\(acc, st) tx ->
                            let ((Sch.FilteredTransactions{..}, _), st') =
                                  Types.runSI
                                    (Sch.filterTransactions blockSize [tx])
                                    (Set.singleton alesAccount)
                                    Types.dummyChainMeta
                                    st
                            in (acc ++ [(ftAdded, ftFailed, st' ^. blockBirkParameters)], st'))
                         ([], initialBlockState)
                         txs
  return (res, state)

tests :: Spec
tests = do
  (results, endState) <- runIO (PR.evalContext Init.initialContextData runWithIntermediateStates)
  describe "Baker transactions." $ do
    specify "Result state satisfies invariant" $
        case invariantBlockState endState of
            Left f -> expectationFailure f
            Right _ -> return ()
    specify "Correct number of transactions" $
        length results == length transactionsInput
    specify "Adding three bakers from initial empty state" $
        case take 3 results of
          [([(_,Types.TxSuccess [Types.BakerAdded 0] _ _)],[],bps1),
           ([(_,Types.TxSuccess [Types.BakerAdded 1] _ _)],[],bps2),
           ([(_,Types.TxSuccess [Types.BakerAdded 2] _ _)],[],bps3)] ->
            Map.keys (bps1 ^. Types.birkCurrentBakers . bakerMap) == [0] &&
            Map.keys (bps2 ^. Types.birkCurrentBakers . bakerMap) == [0,1] &&
            Map.keys (bps3 ^. Types.birkCurrentBakers . bakerMap) == [0,1,2]
          _ -> False

    specify "Remove second baker." $
      case results !! 3 of
        ([(_,Types.TxSuccess [Types.BakerRemoved 1] _ _)], [], bps4) ->
            Map.keys (bps4 ^. Types.birkCurrentBakers . bakerMap) == [0,2]
        _ -> False

    specify "Update third baker's account." $
      -- first check that before the account was thomasAccount, and now it is alesAccount
      case (results !! 3, results !! 4) of
        ((_, _, bps4), ([(_,Types.TxSuccess [Types.BakerAccountUpdated 2 _] _ _)], [], bps5)) ->
          Map.keys (bps5 ^. Types.birkCurrentBakers . bakerMap) == [0,2] &&
          let b2 = (bps5 ^. Types.birkCurrentBakers . bakerMap) Map.! 2
          in b2 ^. bakerAccount == alesAccount &&
             ((bps4 ^. Types.birkCurrentBakers . bakerMap) Map.! 2) ^. bakerAccount == thomasAccount
        _ -> False


    specify "Update first baker's sign key." $
      case (results !! 4, results !! 5) of
        ((_, _, bps5), ([(_,Types.TxSuccess [Types.BakerKeyUpdated 0 _] _ _)], [], bps6)) ->
          Map.keys (bps6 ^. Types.birkCurrentBakers . bakerMap) == [0,2] &&
          let b0 = (bps6 ^. Types.birkCurrentBakers . bakerMap) Map.! 0
          in b0 ^. bakerSignatureVerifyKey == BlockSig.verifyKey (bakerSignKey 3) &&
             ((bps5 ^. Types.birkCurrentBakers . bakerMap) Map.! 0) ^. bakerSignatureVerifyKey == BlockSig.verifyKey (bakerSignKey 0)
        _ -> False
