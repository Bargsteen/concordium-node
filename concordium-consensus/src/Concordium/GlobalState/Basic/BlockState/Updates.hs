{-# LANGUAGE TemplateHaskell, BangPatterns, DeriveFunctor, OverloadedStrings #-}
-- |Implementation of the chain update mechanism: https://concordium.gitlab.io/whitepapers/update-mechanism/main.pdf
module Concordium.GlobalState.Basic.BlockState.Updates where

import Control.Monad
import Data.Aeson as AE
import qualified Data.ByteString as BS
import Data.Foldable
import Data.List.NonEmpty (NonEmpty(..))
import qualified Data.Map as Map
import Data.Semigroup
import Data.Serialize
import Lens.Micro.Platform

import qualified Concordium.Crypto.SHA256 as H
import Concordium.Types
import Concordium.Types.HashableTo
import Concordium.Types.Updates
import Concordium.Utils.Serialization

import Concordium.GlobalState.Parameters

-- |An update queue consists of pending future updates ordered by
-- the time at which they will take effect.
data UpdateQueue e = UpdateQueue {
    -- |The next available sequence number for an update.
    _uqNextSequenceNumber :: !UpdateSequenceNumber,
    -- |Pending updates, in ascending order of effective time.
    _uqQueue :: ![(TransactionTime, e)]
} deriving (Show, Functor, Eq)
makeLenses ''UpdateQueue

instance HashableTo H.Hash e => HashableTo H.Hash (UpdateQueue e) where
    getHash UpdateQueue{..} = H.hash $ runPut $ do
        put _uqNextSequenceNumber
        putLength $ length _uqQueue
        mapM_ (\(t, e) -> put t >> put (getHash e :: H.Hash)) _uqQueue

instance ToJSON e => ToJSON (UpdateQueue e) where
    toJSON UpdateQueue{..} = object [
            "nextSequenceNumber" AE..= _uqNextSequenceNumber,
            "queue" AE..= [object ["effectiveTime" AE..= et, "update" AE..= u] | (et, u) <- _uqQueue]
        ]

instance FromJSON e => FromJSON (UpdateQueue e) where
    parseJSON = withObject "Update queue" $ \o -> do
        _uqNextSequenceNumber <- o AE..: "nextSequenceNumber"
        queue <- o AE..: "queue"
        _uqQueue <- withArray "queue" (\vec -> forM (toList vec) $ withObject "Queue entry" $ \e -> do
                tt <- e AE..: "effectiveTime"
                upd <- e AE..: "update"
                return (tt, upd)
            ) queue
        return UpdateQueue{..}

-- |Update queue with no pending updates, and with the minimal next
-- sequence number.
emptyUpdateQueue :: UpdateQueue e
emptyUpdateQueue = UpdateQueue {
        _uqNextSequenceNumber = minUpdateSequenceNumber,
        _uqQueue = []
    }

-- |Add an update event to an update queue, incrementing the sequence number.
-- Any updates in the queue with later or equal effective times are removed
-- from the queue.
enqueue :: TransactionTime -> e -> UpdateQueue e -> UpdateQueue e
enqueue !t !e = (uqNextSequenceNumber +~ 1)
            . (uqQueue %~ \q -> let !r = takeWhile ((< t) . fst) q in r ++ [(t, e)])

-- |Update queues for all on-chain update types.
data PendingUpdates = PendingUpdates {
    -- |Updates to authorized update keys.
    _pAuthorizationQueue :: !(UpdateQueue (Hashed Authorizations)),
    -- |Protocol updates.
    _pProtocolQueue :: !(UpdateQueue ProtocolUpdate),
    -- |Updates to the election difficulty parameter.
    _pElectionDifficultyQueue :: !(UpdateQueue ElectionDifficulty),
    -- |Updates to the euro:energy exchange rate.
    _pEuroPerEnergyQueue :: !(UpdateQueue ExchangeRate),
    -- |Updates to the GTU:euro exchange rate.
    _pMicroGTUPerEuroQueue :: !(UpdateQueue ExchangeRate),
    -- |Updates to the foundation account.
    _pFoundationAccountQueue :: !(UpdateQueue AccountIndex),
    -- |Updates to the mint distribution.
    _pMintDistributionQueue :: !(UpdateQueue MintDistribution),
    -- |Updates to the transaction fee distribution.
    _pTransactionFeeDistributionQueue :: !(UpdateQueue TransactionFeeDistribution),
    -- |Updates to the GAS rewards.
    _pGASRewardsQueue :: !(UpdateQueue GASRewards)
} deriving (Show, Eq)
makeLenses ''PendingUpdates

instance HashableTo H.Hash PendingUpdates where
    getHash PendingUpdates{..} = H.hash $
            hsh _pAuthorizationQueue
            <> hsh _pProtocolQueue
            <> hsh _pElectionDifficultyQueue
            <> hsh _pEuroPerEnergyQueue
            <> hsh _pMicroGTUPerEuroQueue
            <> hsh _pFoundationAccountQueue
            <> hsh _pMintDistributionQueue
            <> hsh _pTransactionFeeDistributionQueue
            <> hsh _pGASRewardsQueue
        where
            hsh :: HashableTo H.Hash a => a -> BS.ByteString
            hsh = H.hashToByteString . getHash

instance ToJSON PendingUpdates where
    toJSON PendingUpdates{..} = object [
            "authorization" AE..= (_unhashed <$> _pAuthorizationQueue),
            "protocol" AE..= _pProtocolQueue,
            "electionDifficulty" AE..= _pElectionDifficultyQueue,
            "euroPerEnergy" AE..= _pEuroPerEnergyQueue,
            "microGTUPerEuro" AE..= _pMicroGTUPerEuroQueue,
            "foundationAccount" AE..= _pFoundationAccountQueue,
            "mintDistribution" AE..= _pMintDistributionQueue,
            "transactionFeeDistribution" AE..= _pTransactionFeeDistributionQueue,
            "gasRewards" AE..= _pGASRewardsQueue
        ]

instance FromJSON PendingUpdates where
    parseJSON = withObject "PendingUpdates" $ \o -> do
        _pAuthorizationQueue <- fmap makeHashed <$> o AE..: "authorization"
        _pProtocolQueue <- o AE..: "protocol"
        _pElectionDifficultyQueue <- o AE..: "electionDifficulty"
        _pEuroPerEnergyQueue <- o AE..: "euroPerEnergy"
        _pMicroGTUPerEuroQueue <- o AE..: "microGTUPerEuro"
        _pFoundationAccountQueue <- o AE..: "foundationAccount"
        _pMintDistributionQueue <- o AE..: "mintDistribution"
        _pTransactionFeeDistributionQueue <- o AE..: "transactionFeeDistribution"
        _pGASRewardsQueue <- o AE..: "gasRewards"
        return PendingUpdates{..}

-- |Initial pending updates with empty queues.
emptyPendingUpdates :: PendingUpdates
emptyPendingUpdates = PendingUpdates
        emptyUpdateQueue 
        emptyUpdateQueue 
        emptyUpdateQueue
        emptyUpdateQueue
        emptyUpdateQueue
        emptyUpdateQueue
        emptyUpdateQueue
        emptyUpdateQueue
        emptyUpdateQueue

-- |Current state of updatable parameters and update queues.
data Updates = Updates {
    -- |Current update authorizations.
    _currentAuthorizations :: !(Hashed Authorizations),
    -- |Current protocol update.
    _currentProtocolUpdate :: !(Maybe ProtocolUpdate),
    -- |Current chain parameters.
    _currentParameters :: !ChainParameters,
    -- |Pending updates.
    _pendingUpdates :: !PendingUpdates
} deriving (Show, Eq)
makeClassy ''Updates

instance HashableTo H.Hash Updates where
    getHash Updates{..} = H.hash $
            hsh _currentAuthorizations
            <> case _currentProtocolUpdate of
                    Nothing -> "\x00"
                    Just cpu -> "\x01" <> hsh cpu
            <> hsh _currentParameters
            <> hsh _pendingUpdates
        where
            hsh :: HashableTo H.Hash a => a -> BS.ByteString
            hsh = H.hashToByteString . getHash

instance ToJSON Updates where
    toJSON Updates{..} = object $ [
            "authorizations" AE..= _unhashed _currentAuthorizations,
            "chainParameters" AE..= _currentParameters,
            "updateQueues" AE..= _pendingUpdates
        ] <> toList (("protocolUpdate" AE..=) <$> _currentProtocolUpdate)

instance FromJSON Updates where
    parseJSON = withObject "Updates" $ \o -> do
        _currentAuthorizations <- makeHashed <$> o AE..: "authorizations"
        _currentProtocolUpdate <- o AE..:? "protocolUpdate"
        _currentParameters <- o AE..: "chainParameters"
        _pendingUpdates <- o AE..: "updateQueues"
        return Updates{..}

-- |An initial 'Updates' with the given initial 'Authorizations'
-- and 'ChainParameters'.
initialUpdates :: Authorizations -> ChainParameters -> Updates
initialUpdates initialAuthorizations _currentParameters = Updates {
        _currentAuthorizations = makeHashed initialAuthorizations,
        _currentProtocolUpdate = Nothing,
        _pendingUpdates = emptyPendingUpdates,
        ..
    }

-- |Process the update queue to determine the new value of a parameter (or the authorizations).
-- This splits the queue at the given timestamp. The last value up to and including the timestamp
-- is the new value, if any -- otherwise the current value is retained. The queue is updated to
-- be the updates with later timestamps.
processValueUpdates ::
    Timestamp
    -- ^Current timestamp
    -> v
    -- ^Current value
    -> UpdateQueue v
    -- ^Current queue
    -> (v, UpdateQueue v, Map.Map TransactionTime v)
processValueUpdates t a0 uq = (getLast (sconcat (Last <$> a0 :| (snd <$> ql))), uq {_uqQueue = qr}, Map.fromAscList ql)
    where
        (ql, qr) = span ((<= t) . transactionTimeToTimestamp . fst) (uq ^. uqQueue)

-- |Process the protocol update queue.  Unlike other queues, once a protocol update occurs, it is not
-- overridden by later ones.
-- FIXME: We may just want to keep unused protocol updates in the queue, even if their timestamps have
-- elapsed.
processProtocolUpdates :: Timestamp -> Maybe ProtocolUpdate -> UpdateQueue ProtocolUpdate -> (Maybe ProtocolUpdate, UpdateQueue ProtocolUpdate, Map.Map TransactionTime ProtocolUpdate)
processProtocolUpdates t pu0 uq = (pu', uq {_uqQueue = qr}, Map.fromAscList ql)
    where
        (ql, qr) = span ((<= t) . transactionTimeToTimestamp . fst) (uq ^. uqQueue)
        pu' = getFirst <$> mconcat ((First <$> pu0) : (Just . First . snd <$> ql))


-- |Process the update queues to determine the current state of
-- updates.
processUpdateQueues
    :: Timestamp
    -- ^Current timestamp
    -> Updates
    -> (Map.Map TransactionTime UpdateValue, Updates)
processUpdateQueues t Updates{_pendingUpdates = PendingUpdates{..}, _currentParameters = ChainParameters{_cpRewardParameters=RewardParameters{..}, ..}, ..} = (res, Updates {
            _currentAuthorizations = newAuthorizations,
            _currentProtocolUpdate = newProtocolUpdate,
            _currentParameters = makeChainParameters
                        newElectionDifficulty
                        newEuroPerEnergy
                        newMicroGTUPerEuro
                        _cpBakerExtraCooldownEpochs
                        _cpAccountCreationLimit
                        RewardParameters {
                            _rpMintDistribution = newMintDistribution,
                            _rpTransactionFeeDistribution = newTransactionFeeDistribution,
                            _rpGASRewards = newGASRewards
                        }
                        newFoundationAccount,
            _pendingUpdates = PendingUpdates {
                    _pAuthorizationQueue = newAuthorizationQueue,
                    _pProtocolQueue = newProtocolQueue,
                    _pElectionDifficultyQueue = newElectionDifficultyQueue,
                    _pEuroPerEnergyQueue = newEuroPerEnergyQueue,
                    _pMicroGTUPerEuroQueue = newMicroGTUPerEuroQueue,
                    _pFoundationAccountQueue = newFoundationAccountQueue,
                    _pMintDistributionQueue = newMintDistributionQueue,
                    _pTransactionFeeDistributionQueue = newTransactionFeeDistributionQueue,
                    _pGASRewardsQueue = newGASRewardsQueue
                }
        })
    where
        (newAuthorizations, newAuthorizationQueue, resAuthorization) = processValueUpdates t _currentAuthorizations _pAuthorizationQueue
        (newProtocolUpdate, newProtocolQueue, resProtocol) = processProtocolUpdates t _currentProtocolUpdate _pProtocolQueue
        (newElectionDifficulty, newElectionDifficultyQueue, resElectionDifficulty) = processValueUpdates t _cpElectionDifficulty _pElectionDifficultyQueue
        (newEuroPerEnergy, newEuroPerEnergyQueue, resEuroPerEnergy) = processValueUpdates t _cpEuroPerEnergy _pEuroPerEnergyQueue
        (newMicroGTUPerEuro, newMicroGTUPerEuroQueue, resMicroGTUPerEuro) = processValueUpdates t _cpMicroGTUPerEuro _pMicroGTUPerEuroQueue
        (newFoundationAccount, newFoundationAccountQueue, resFoundationAccount) = processValueUpdates t _cpFoundationAccount _pFoundationAccountQueue
        (newMintDistribution, newMintDistributionQueue, resMintDistribution) = processValueUpdates t _rpMintDistribution _pMintDistributionQueue
        (newTransactionFeeDistribution, newTransactionFeeDistributionQueue, resTransactionFeeDistribution) = processValueUpdates t _rpTransactionFeeDistribution _pTransactionFeeDistributionQueue
        (newGASRewards, newGASRewardsQueue, resGASRewards) = processValueUpdates t _rpGASRewards _pGASRewardsQueue
        res = 
            (UVAuthorization . _unhashed <$> resAuthorization) <>
            (UVProtocol <$> resProtocol) <>
            (UVElectionDifficulty <$> resElectionDifficulty) <>
            (UVEuroPerEnergy <$> resEuroPerEnergy) <>
            (UVMicroGTUPerEuro <$> resMicroGTUPerEuro) <>
            (UVFoundationAccount <$> resFoundationAccount) <>
            (UVMintDistribution <$> resMintDistribution) <>
            (UVTransactionFeeDistribution <$> resTransactionFeeDistribution) <>
            (UVGASRewards <$> resGASRewards)

-- |Determine the future election difficulty (at a given time) based
-- on a current 'Updates'.
futureElectionDifficulty :: Updates -> Timestamp -> ElectionDifficulty
futureElectionDifficulty Updates{_pendingUpdates = PendingUpdates{..},..} ts
        = processValueUpdates ts (_cpElectionDifficulty _currentParameters) _pElectionDifficultyQueue ^. _1

-- |Get the protocol update status: either an effective protocol update or
-- a list of pending future protocol updates.
protocolUpdateStatus :: Updates -> Either ProtocolUpdate [(TransactionTime, ProtocolUpdate)]
protocolUpdateStatus Updates{_pendingUpdates = PendingUpdates{..},..}
        = case _currentProtocolUpdate of
            Nothing -> Right (_uqQueue _pProtocolQueue)
            Just pu -> Left pu

-- |Determine the next sequence number for a given update type.
lookupNextUpdateSequenceNumber :: Updates -> UpdateType -> UpdateSequenceNumber
lookupNextUpdateSequenceNumber u UpdateAuthorization = u ^. pendingUpdates . pAuthorizationQueue . uqNextSequenceNumber
lookupNextUpdateSequenceNumber u UpdateProtocol = u ^. pendingUpdates . pProtocolQueue . uqNextSequenceNumber
lookupNextUpdateSequenceNumber u UpdateElectionDifficulty = u ^. pendingUpdates . pElectionDifficultyQueue . uqNextSequenceNumber
lookupNextUpdateSequenceNumber u UpdateEuroPerEnergy = u ^. pendingUpdates . pEuroPerEnergyQueue . uqNextSequenceNumber
lookupNextUpdateSequenceNumber u UpdateMicroGTUPerEuro = u ^. pendingUpdates . pMicroGTUPerEuroQueue . uqNextSequenceNumber
lookupNextUpdateSequenceNumber u UpdateFoundationAccount = u ^. pendingUpdates . pFoundationAccountQueue . uqNextSequenceNumber
lookupNextUpdateSequenceNumber u UpdateMintDistribution = u ^. pendingUpdates . pMintDistributionQueue . uqNextSequenceNumber
lookupNextUpdateSequenceNumber u UpdateTransactionFeeDistribution = u ^. pendingUpdates . pTransactionFeeDistributionQueue . uqNextSequenceNumber
lookupNextUpdateSequenceNumber u UpdateGASRewards = u ^. pendingUpdates . pGASRewardsQueue . uqNextSequenceNumber

-- |Enqueue an update in the appropriate queue.
enqueueUpdate :: TransactionTime -> UpdateValue -> Updates -> Updates
enqueueUpdate effectiveTime (UVAuthorization auths) = pendingUpdates . pAuthorizationQueue %~ enqueue effectiveTime (makeHashed auths)
enqueueUpdate effectiveTime (UVProtocol protUp) = pendingUpdates . pProtocolQueue %~ enqueue effectiveTime protUp
enqueueUpdate effectiveTime (UVElectionDifficulty edUp) = pendingUpdates . pElectionDifficultyQueue %~ enqueue effectiveTime edUp
enqueueUpdate effectiveTime (UVEuroPerEnergy epeUp) = pendingUpdates . pEuroPerEnergyQueue %~ enqueue effectiveTime epeUp
enqueueUpdate effectiveTime (UVMicroGTUPerEuro mgtupeUp) = pendingUpdates . pMicroGTUPerEuroQueue %~ enqueue effectiveTime mgtupeUp
enqueueUpdate effectiveTime (UVFoundationAccount upd) = pendingUpdates . pFoundationAccountQueue %~ enqueue effectiveTime upd
enqueueUpdate effectiveTime (UVMintDistribution upd) = pendingUpdates . pMintDistributionQueue %~ enqueue effectiveTime upd
enqueueUpdate effectiveTime (UVTransactionFeeDistribution upd) = pendingUpdates . pTransactionFeeDistributionQueue %~ enqueue effectiveTime upd
enqueueUpdate effectiveTime (UVGASRewards upd) = pendingUpdates . pGASRewardsQueue %~ enqueue effectiveTime upd