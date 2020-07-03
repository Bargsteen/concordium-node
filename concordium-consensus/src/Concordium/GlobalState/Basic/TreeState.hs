{-# LANGUAGE TypeFamilies #-}
{-# LANGUAGE TemplateHaskell #-}
{-# LANGUAGE StandaloneDeriving #-}
{-# LANGUAGE UndecidableInstances #-}
module Concordium.GlobalState.Basic.TreeState where

import Lens.Micro.Platform
import Concordium.Utils
import Data.List as List
import Data.Foldable
import Control.Monad.State
import Control.Exception
import Data.Functor.Identity

import qualified Data.Map.Strict as Map
import qualified Data.HashMap.Strict as HM
import qualified Data.Sequence as Seq
import qualified Data.PQueue.Prio.Min as MPQ
import qualified Data.Set as Set

import Concordium.GlobalState.Types
import Concordium.GlobalState.Basic.BlockPointer
import Concordium.GlobalState.Block
import Concordium.GlobalState.BlockMonads
import Concordium.GlobalState.BlockPointer
import qualified Concordium.GlobalState.BlockState as BS
import Concordium.GlobalState.Finalization
import Concordium.GlobalState.Parameters
import Concordium.GlobalState.TransactionTable
import Concordium.GlobalState.Statistics (ConsensusStatistics, initialConsensusStatistics)
import qualified Concordium.GlobalState.TreeState as TS
import Concordium.Types
import Concordium.Types.HashableTo
import Concordium.Types.Transactions
import Concordium.GlobalState.AccountTransactionIndex
import qualified Data.HashMap.Strict
import qualified Data.Foldable as Fold

data SkovData bs = SkovData {
    -- |Map of all received blocks by hash.
    _blockTable :: !(HM.HashMap BlockHash (TS.BlockStatus (BasicBlockPointer bs) PendingBlock)),
    -- |Table of blocks finalized by height.
    _finalizedByHeightTable :: !(HM.HashMap BlockHeight (BasicBlockPointer bs)),
    -- |Map of (possibly) pending blocks by hash
    _possiblyPendingTable :: !(HM.HashMap BlockHash [PendingBlock]),
    -- |Priority queue of pairs of (block, parent) hashes where the block is (possibly) pending its parent, by block slot
    _possiblyPendingQueue :: !(MPQ.MinPQueue Slot (BlockHash, BlockHash)),
    -- |List of finalization records with the blocks that they finalize, starting from genesis
    _finalizationList :: !(Seq.Seq (FinalizationRecord, BasicBlockPointer bs)),
    -- |Branches of the tree by height above the last finalized block
    _branches :: !(Seq.Seq [BasicBlockPointer bs]),
    -- |Genesis data
    _genesisData :: !GenesisData,
    -- |Block pointer to genesis block
    _genesisBlockPointer :: !(BasicBlockPointer bs),
    -- |Current focus block
    _focusBlock :: !(BasicBlockPointer bs),
    -- |Pending transaction table
    _pendingTransactions :: !PendingTransactionTable,
    -- |Transaction table
    _transactionTable :: !TransactionTable,
    -- |Consensus statistics
    _statistics :: !ConsensusStatistics,
    -- |Runtime parameters
    _runtimeParameters :: !RuntimeParameters,
    -- |Transaction table purge counter
    _transactionTablePurgeCounter :: !Int
}
makeLenses ''SkovData

instance Show (SkovData bs) where
    show SkovData{..} = "Finalized: " ++ intercalate "," (take 6 . show . bpHash . snd <$> toList _finalizationList) ++ "\n" ++
        "Branches: " ++ intercalate "," ( (('[':) . (++"]") . intercalate "," . map (take 6 . show . bpHash)) <$> toList _branches)

-- |Initial skov data with default runtime parameters (block size = 10MB).
initialSkovDataDefault :: GenesisData -> bs -> SkovData bs
initialSkovDataDefault = initialSkovData defaultRuntimeParameters

initialSkovData :: RuntimeParameters -> GenesisData -> bs -> SkovData bs
initialSkovData rp gd genState =
  SkovData {
            _blockTable = HM.singleton gbh (TS.BlockFinalized gb gbfin),
            _finalizedByHeightTable = HM.singleton 0 gb,
            _possiblyPendingTable = HM.empty,
            _possiblyPendingQueue = MPQ.empty,
            _finalizationList = Seq.singleton (gbfin, gb),
            _branches = Seq.empty,
            _genesisData = gd,
            _genesisBlockPointer = gb,
            _focusBlock = gb,
            _pendingTransactions = emptyPendingTransactionTable,
            _transactionTable = emptyTransactionTable,
            _statistics = initialConsensusStatistics,
            _runtimeParameters = rp,
            _transactionTablePurgeCounter = 0
        }
  where gbh = bpHash gb
        gbfin = FinalizationRecord 0 gbh emptyFinalizationProof 0
        gb = makeGenesisBasicBlockPointer gd genState

-- |Newtype wrapper that provides an implementation of the TreeStateMonad using a non-persistent tree state.
-- The underlying Monad must provide instances for:
--
-- * `BlockStateTypes`
-- * `BlockStateQuery`
-- * `BlockStateOperations`
-- * `BlockStateStorage`
-- * `MonadState (SkovData bs)`
--
-- This newtype establishes types for the @GlobalStateTypes@. The type variable @bs@ stands for the BlockState
-- type used in the implementation.
newtype PureTreeStateMonad bs m a = PureTreeStateMonad { runPureTreeStateMonad :: m a }
  deriving (Functor, Applicative, Monad, MonadIO, BlockStateTypes,
            BS.BlockStateQuery, BS.BakerQuery, BS.BlockStateOperations, BS.BlockStateStorage, BS.BirkParametersOperations)

deriving instance (Monad m, MonadState (SkovData bs) m) => MonadState (SkovData bs) (PureTreeStateMonad bs m)


instance (bs ~ BlockState m) => GlobalStateTypes (PureTreeStateMonad bs m) where
    type BlockPointerType (PureTreeStateMonad bs m) = BasicBlockPointer bs

instance (bs ~ BlockState m, Monad m, MonadState (SkovData bs) m) => BlockPointerMonad (PureTreeStateMonad bs m) where
    blockState = return . _bpState
    bpParent = return . runIdentity . _bpParent
    bpLastFinalized = return . runIdentity . _bpLastFinalized

instance ATITypes (PureTreeStateMonad bs m) where
  type ATIStorage (PureTreeStateMonad bs m) = ()

instance (Monad m) => PerAccountDBOperations (PureTreeStateMonad bs m) where
  -- default instance because ati = ()

instance (bs ~ BlockState m, BS.BlockStateStorage m, Monad m, MonadIO m, MonadState (SkovData bs) m)
          => TS.TreeStateMonad (PureTreeStateMonad bs m) where
    makePendingBlock key slot parent bid pf n lastFin trs time = do
        return $ makePendingBlock (signBlock key slot parent bid pf n lastFin trs) time
    getBlockStatus bh = use (blockTable . at' bh)
    makeLiveBlock block parent lastFin st () arrTime energy = do
            let blockP = makeBasicBlockPointer block parent lastFin st arrTime energy
            blockTable . at' (getHash block) ?= TS.BlockAlive blockP
            return blockP
    markDead bh = blockTable . at' bh ?= TS.BlockDead
    markFinalized bh fr = use (blockTable . at' bh) >>= \case
            Just (TS.BlockAlive bp) -> do
              blockTable . at' bh ?= TS.BlockFinalized bp fr
              finalizedByHeightTable . at (bpHeight bp) ?= bp
            _ -> return ()
    markPending pb = blockTable . at' (getHash pb) ?= TS.BlockPending pb
    getGenesisBlockPointer = use genesisBlockPointer
    getGenesisData = use genesisData
    getLastFinalized = use finalizationList >>= \case
            _ Seq.:|> (finRec,lf) -> return (lf, finRec)
            _ -> error "empty finalization list"
    getNextFinalizationIndex = FinalizationIndex . fromIntegral . Seq.length <$> use finalizationList
    addFinalization newFinBlock finRec = finalizationList %= (Seq.:|> (finRec, newFinBlock))
    getFinalizedAtIndex finIndex = fmap snd . Seq.lookup (fromIntegral finIndex) <$> use finalizationList
    getRecordAtIndex finIndex = fmap fst . Seq.lookup (fromIntegral finIndex) <$> use finalizationList
    getFinalizedAtHeight bHeight = preuse (finalizedByHeightTable . ix bHeight)
    getBranches = use branches
    putBranches brs = branches .= brs
    takePendingChildren bh = possiblyPendingTable . at' bh . non [] <<.= []
    addPendingBlock pb = do
        let parent = blockPointer (bbFields (pbBlock pb))
        possiblyPendingTable . at' parent . non [] %= (pb:)
        possiblyPendingQueue %= MPQ.insert (blockSlot (pbBlock pb)) (getHash pb, parent)
    takeNextPendingUntil slot = tnpu =<< use possiblyPendingQueue
        where
            tnpu ppq = case MPQ.minViewWithKey ppq of
                Just ((sl, (pbh, parenth)), ppq') ->
                    if sl <= slot then do
                        (myPB, otherPBs) <- partition ((== pbh) . pbHash) <$> use (possiblyPendingTable . at' parenth . non [])
                        case myPB of
                            [] -> tnpu ppq'
                            (realPB : _) -> do
                                possiblyPendingTable . at' parenth . non [] .= otherPBs
                                possiblyPendingQueue .= ppq'
                                return (Just realPB)
                    else do
                        possiblyPendingQueue .= ppq
                        return Nothing
                Nothing -> do
                    possiblyPendingQueue .= ppq
                    return Nothing
    getFocusBlock = use focusBlock
    putFocusBlock bb = focusBlock .= bb
    getPendingTransactions = use pendingTransactions
    putPendingTransactions pts = pendingTransactions .= pts
    getAccountNonFinalized addr nnce =
            use (transactionTable . ttNonFinalizedTransactions . at' addr) >>= \case
                Nothing -> return []
                Just anfts ->
                    let (_, atnnce, beyond) = Map.splitLookup nnce (anfts ^. anftMap)
                    in return $ case atnnce of
                        Nothing -> Map.toAscList beyond
                        Just s -> (nnce, s) : Map.toAscList beyond

    getNextAccountNonce addr =
        use (transactionTable . ttNonFinalizedTransactions . at' addr) >>= \case
                Nothing -> return (minNonce, True)
                Just anfts ->
                  case Map.lookupMax (anfts ^. anftMap) of
                    Nothing -> return (anfts ^. anftNextNonce, True) -- all transactions are finalized
                    Just (nonce, _) -> return (nonce + 1, False)

    getCredential txHash =
      preuse (transactionTable . ttHashMap . ix txHash) >>= \case
        Just (WithMetadata{wmdData=CredentialDeployment{..},..}, _) -> return $! Just WithMetadata{wmdData=biCred,..}
        _ -> return Nothing

    addCommitTransaction bi@WithMetadata{..} slot = do
      let trHash = wmdHash
      tt <- use transactionTable
      case tt ^. ttHashMap . at' trHash of
          Nothing ->
            case wmdData of
              NormalTransaction tr -> do
                let sender = transactionSender tr
                    nonce = transactionNonce tr
                if (tt ^. ttNonFinalizedTransactions . at' sender . non emptyANFT . anftNextNonce) <= nonce then do
                  transactionTablePurgeCounter %= (+ 1)
                  let wmdtr = WithMetadata{wmdData=tr,..}
                  transactionTable .= (tt & (ttNonFinalizedTransactions . at' sender . non emptyANFT . anftMap . at' nonce . non Set.empty %~ Set.insert wmdtr)
                                          & (ttHashMap . at' trHash ?~ (bi, Received slot)))
                  return (TS.Added bi)
                else return TS.ObsoleteNonce
              CredentialDeployment{..} -> do
                transactionTable . ttHashMap . at' trHash ?= (bi, Received slot)
                return (TS.Added bi)
          Just (_, Finalized{}) ->
            return TS.ObsoleteNonce
          Just (tr', results) -> do
            when (slot > results ^. tsSlot) $ transactionTable . ttHashMap . at' trHash . mapped . _2 %= updateSlot slot
            return $ TS.Duplicate tr'

    finalizeTransactions bh slot = mapM_ finTrans
        where
            finTrans WithMetadata{wmdData=NormalTransaction tr,..} = do
                let nonce = transactionNonce tr
                    sender = transactionSender tr
                anft <- use (transactionTable . ttNonFinalizedTransactions . at' sender . non emptyANFT)
                assert (anft ^. anftNextNonce == nonce) $ do
                    let nfn = anft ^. anftMap . at' nonce . non Set.empty
                    let wmdtr = WithMetadata{wmdData=tr,..}
                    assert (Set.member wmdtr nfn) $ do
                        -- Remove any other transactions with this nonce from the transaction table.
                        -- They can never be part of any other block after this point.
                        forM_ (Set.delete wmdtr nfn) $
                          \deadTransaction -> transactionTable . ttHashMap . at' (getHash deadTransaction) .= Nothing
                        -- Mark the status of the transaction as finalized.
                        -- Singular here is safe due to the precondition (and assertion) that all transactions
                        -- which are part of live blocks are in the transaction table.
                        transactionTable . ttHashMap . singular (ix wmdHash) . _2 %=
                            \case Committed{..} -> Finalized{_tsSlot=slot,tsBlockHash=bh,tsFinResult=tsResults HM.! bh,..}
                                  _ -> error "Transaction should be in committed state when finalized."
                        -- Update the non-finalized transactions for the sender
                        transactionTable . ttNonFinalizedTransactions . at' sender ?= (anft & (anftMap . at' nonce .~ Nothing) & (anftNextNonce .~ nonce + 1))
            finTrans WithMetadata{wmdData=CredentialDeployment{..},..} = do
              transactionTable . ttHashMap . singular (ix wmdHash) . _2 %=
                            \case Committed{..} -> Finalized{_tsSlot=slot,tsBlockHash=bh,tsFinResult=tsResults HM.! bh,..}
                                  _ -> error "Transaction should be in committed state when finalized."

    commitTransaction slot bh tr idx =
        transactionTable . ttHashMap . at' (getHash tr) %= fmap (_2 %~ addResult bh slot idx)

    purgeTransaction WithMetadata{..} =
        use (transactionTable . ttHashMap . at' wmdHash) >>= \case
            Nothing -> return True
            Just (_, results) -> do
                lastFinSlot <- blockSlot . _bpBlock . fst <$> TS.getLastFinalized
                if (lastFinSlot >= results ^. tsSlot) then do
                    -- remove from the table
                    transactionTable . ttHashMap . at' wmdHash .= Nothing
                    -- if the transaction is from a sender also delete the relevant
                    -- entry in the account non finalized table
                    case wmdData of
                      NormalTransaction tr -> do
                        let nonce = transactionNonce tr
                            sender = transactionSender tr
                        transactionTable
                          . ttNonFinalizedTransactions
                          . at' sender
                          . non emptyANFT
                          . anftMap
                          . at' nonce
                          . non Set.empty %= Set.delete WithMetadata{wmdData=tr,..}
                      _ -> return () -- do nothing.
                    return True
                else return False

    markDeadTransaction bh tr =
      -- We only need to update the outcomes. The anf table nor the pending table need be updated
      -- here since a transaction should not be marked dead in a finalized block.
      transactionTable . ttHashMap . at' (getHash tr) . mapped . _2 %= markDeadResult bh
    lookupTransaction th =
       preuse (transactionTable . ttHashMap . ix th . _2)

    getConsensusStatistics = use statistics
    putConsensusStatistics stats = statistics .= stats

    {-# INLINE getRuntimeParameters #-}
    getRuntimeParameters = use runtimeParameters

    purgeTransactionTable ignoreInsertions currentTime = do
      purgeCount <- use transactionTablePurgeCounter
      RuntimeParameters{..} <- use runtimeParameters
      when (ignoreInsertions || purgeCount > rpInsertionsBeforeTransactionPurge) $ do
        transactionTablePurgeCounter .= 0
        lastFinalizedSlot <- blockSlot <$> (use finalizationList >>= \case
                                            _ Seq.:|> (_, lf) -> return lf
                                            _ -> error "empty finalization list")
        transactionTable' <- use transactionTable
        pendingTransactions' <- use pendingTransactions

        txHighestNonces <- removeTransactions transactionTable' rpTransactionsKeepAliveTime lastFinalizedSlot
        pendingTransactions .= rollbackNonces txHighestNonces pendingTransactions'
     where
       removeTransactions TransactionTable{..} keepAliveTime lastFinalizedSlot = do
         let removeTxs :: [(Nonce, Set.Set Transaction)] -> Nonce -> ([Transaction], [(Nonce, Set.Set Transaction)], Nonce)
             removeTxs [] h = ([], [], h) -- If we have no more transactions to process, return the same value as before.
             removeTxs ((thisNonce, transactionsForThisNonce):setsOfTxsByNonce) highestNonce =
               -- We can only remove transactions which are not committed to any blocks.
               -- Otherwise we would break many invariants.
               let removable t =
                     case _ttHashMap ^? ix (biHash t) . _2 of
                       -- we cannot remove a transaction that was received in a block that has not yet been purged
                       -- if its received slot is >= last finalized then the transaction will be in a live block
                       -- that might be processed at some point.
                       Just Received{..} -> _tsSlot <= lastFinalizedSlot
                       _ -> False
                   (transactionsToDrop, transactionsToKeep) =
                     Set.partition (\t -> biArrivalTime t + keepAliveTime < utcTimeToTransactionTime currentTime && removable t) transactionsForThisNonce in -- split in old and still valid transactions
                 if Set.size transactionsToKeep == 0
                 then
                    -- If we don't keep any transactions for this nonce, because:
                    -- - They all expired
                    -- - They were not included in a block that was finalized or still live
                    -- then we can assume that transactions with higher nonces should be dropped.
                    (Set.elems transactionsToDrop ++ concatMap (Set.elems . snd) setsOfTxsByNonce, [], highestNonce)
                 else
                    -- else we continue purging the next transactions
                    let (nextToDrop, nextToKeep, nextHighestNonce) = removeTxs setsOfTxsByNonce thisNonce in
                     -- and then combine the transactions to drop and the transactions to keep that we already had with the ones from the next step
                     (Set.elems transactionsToDrop ++ nextToDrop, (thisNonce, transactionsToKeep) : nextToKeep, nextHighestNonce)

             -- Given an AccountNonFinalizedTransactions, generate:
             -- 1. A list of transactions that should be removed
             -- 2. A map of Nonce -> Set Transaction that will be the new anftMap, i.e. the transactions that will be kept for this account.
             -- 3. The new last finalized nonce for this account.
             purgeANFT :: (AccountAddress, AccountNonFinalizedTransactions) -> ([Transaction], (AccountAddress, AccountNonFinalizedTransactions), (AccountAddress, Nonce, Bool))
             purgeANFT (acc, AccountNonFinalizedTransactions{..}) =
               let (transactionsToRemove, transactionsToKeep, highestNonce) = removeTxs (Map.toAscList _anftMap) 0 in
                 (transactionsToRemove, (acc, AccountNonFinalizedTransactions{_anftMap = Map.fromList transactionsToKeep, ..}), (acc, highestNonce, null transactionsToKeep))

         let results = map purgeANFT (HM.toList _ttNonFinalizedTransactions)
             allTransactionsToRemove = concatMap (^. _1) results
             newNFT = Data.HashMap.Strict.fromList $ map (^. _2) results
             highestNonces = map (^. _3) results
             -- remove all normal transactions that should be removed
             newTMap = Fold.foldl' (\h tx -> (HM.delete (biHash tx) h)) _ttHashMap allTransactionsToRemove
             -- and finally remove all the credential deployments that are too old.
             finalTT = HM.filter (\case
                                      (WithMetadata{wmdData=CredentialDeployment{},..}, Received{..}) ->
                                          wmdArrivalTime + keepAliveTime >= utcTimeToTransactionTime currentTime && _tsSlot > lastFinalizedSlot
                                      _ -> True
                                  ) newTMap
         transactionTable .= TransactionTable{_ttHashMap = finalTT, _ttNonFinalizedTransactions = newNFT}
         return highestNonces

       rollbackNonces :: [(AccountAddress, Nonce, Bool)] -> PendingTransactionTable -> PendingTransactionTable
       rollbackNonces e PTT{..} = PTT {_pttWithSender =
                                       let v = Fold.foldl' (\pt (acc, n, remove) ->
                                                               if remove then Data.HashMap.Strict.delete acc pt
                                                               else
                                                                 Data.HashMap.Strict.update (\(n1, n2) ->
                                                                         if n2 > n && n >= n1 then Just (n1, n)
                                                                         else if n2 > n then Nothing
                                                                         else Just (n1, n2)) acc pt) _pttWithSender e in v,
                                       ..}
