{-# LANGUAGE
    RecordWildCards,
    ScopedTypeVariables,
    TemplateHaskell,
    LambdaCase,
    FlexibleContexts,
    MultiParamTypeClasses,
    FlexibleInstances,
    FunctionalDependencies,
    RankNTypes,
    DerivingStrategies,
    DerivingVia,
    StandaloneDeriving,
    ConstraintKinds,
    GeneralizedNewtypeDeriving,
    UndecidableInstances,
    TypeFamilies
    #-}
module Concordium.Afgjort.Finalize (
    FinalizationStateMonad,
    FinalizationMonad(..),
    FinalizationStateLenses(..),
    FinalizationInstance(..),
    FinalizationState(..),
    FinalizationSessionId(..),
    FinalizationMessage(..),
    FinalizationPseudoMessage(..),
    FinalizationMessageHeader,
    initialFinalizationState,
    initialPassiveFinalizationState,
    verifyFinalProof,
    makeFinalizationCommittee,
    nextFinalizationRecord,
    ActiveFinalizationM(..),
    -- * For testing
    FinalizationRound(..),
    -- TODO: Remove if unneeded
    nextFinalizationJustifierHeight
) where

import qualified Data.Vector as Vec
import qualified Data.Map.Strict as Map
import Data.Map.Strict(Map)
import qualified Data.Set as Set
import Data.Set(Set)
import Data.Maybe
import Lens.Micro.Platform
import Control.Monad.State.Class
import Control.Monad.State.Strict (runState)
import Control.Monad.Reader.Class
import Control.Monad.IO.Class
import Control.Monad
import Data.Bits
import Data.Time.Clock
import qualified Data.OrdPSQ as PSQ

import qualified Concordium.Crypto.BlockSignature as Sig
import qualified Concordium.Crypto.BlsSignature as Bls
import qualified Concordium.Crypto.VRF as VRF
import Concordium.Types
import Concordium.GlobalState.Bakers
import Concordium.GlobalState.Parameters
import Concordium.GlobalState.BlockPointer hiding (BlockPointer)
import Concordium.GlobalState.AccountTransactionIndex
import Concordium.GlobalState.BlockMonads
import Concordium.GlobalState.Finalization
import Concordium.GlobalState.TreeState
import Concordium.GlobalState.BlockState
import Concordium.Kontrol
import Concordium.Afgjort.Types
import Concordium.Afgjort.WMVBA
import Concordium.Afgjort.Freeze (FreezeMessage(..))
import Concordium.Afgjort.FinalizationQueue
import Concordium.Kontrol.BestBlock
import Concordium.Logger
import Concordium.Afgjort.Finalize.Types
import Concordium.Afgjort.Monad
import Concordium.TimeMonad
import Concordium.TimerMonad

atStrict :: (Ord k) => k -> Lens' (Map k v) (Maybe v)
atStrict k f m = f mv <&> \case
        Nothing -> maybe m (const (Map.delete k m)) mv
        Just v' -> Map.insert k v' m
    where mv = Map.lookup k m
{-# INLINE atStrict #-}

data FinalizationRound = FinalizationRound {
    roundInput :: !(Maybe BlockHash),
    roundDelta :: !BlockHeight,
    roundMe :: !Party,
    roundWMVBA :: !(WMVBAState Sig.Signature)
}

instance Show FinalizationRound where
    show FinalizationRound{..} = "roundInput: " ++ take 11 (show roundInput) ++ " roundDelta: " ++ show roundDelta

data PassiveFinalizationRound = PassiveFinalizationRound {
    passiveWitnesses :: Map BlockHeight WMVBAPassiveState
} deriving (Show)

initialPassiveFinalizationRound :: PassiveFinalizationRound
initialPassiveFinalizationRound = PassiveFinalizationRound Map.empty

ancestorAtHeight :: (GlobalStateTypes m, BlockPointerMonad m) => BlockHeight -> BlockPointerType m -> m (BlockPointerType m)
ancestorAtHeight h bp
    | h == bpHeight bp = return bp
    | h < bpHeight bp = do
        parent <- bpParent bp
        ancestorAtHeight h parent
    | otherwise = error "ancestorAtHeight: block is below required height"

-- TODO: Only store pending messages for at most one round in the future.
-- TODO: Revise what pending messages we store. Catch-up no longer is based
-- on pending messages.

data PendingMessage = PendingMessage !Party !WMVBAMessage !Sig.Signature
    deriving (Eq, Ord, Show)

type PendingMessageMap = Map FinalizationIndex (Map BlockHeight (Set PendingMessage))

data FinalizationState timer = FinalizationState {
    _finsSessionId :: !FinalizationSessionId,
    _finsIndex :: !FinalizationIndex,
    _finsHeight :: !BlockHeight,
    _finsIndexInitialDelta :: !BlockHeight,
    _finsCommittee :: !FinalizationCommittee,
    _finsMinSkip :: !BlockHeight,
    _finsPendingMessages :: !PendingMessageMap,
    _finsCurrentRound :: !(Either PassiveFinalizationRound FinalizationRound),
    _finsFailedRounds :: [Map Party Sig.Signature],
    _finsCatchUpTimer :: !(Maybe timer),
    _finsCatchUpAttempts :: !Int,
    _finsCatchUpDeDup :: !(PSQ.OrdPSQ Sig.Signature UTCTime ()),
    _finsQueue :: !FinalizationQueue
}
makeLenses ''FinalizationState

instance Show (FinalizationState timer) where
    show FinalizationState{..} = "finIndex: " ++ show (theFinalizationIndex _finsIndex) ++ " finHeight: " ++ show (theBlockHeight _finsHeight) ++ " currentRound:" ++ show _finsCurrentRound
        ++ "\n pendingMessages:" ++ show (Map.toList $ fmap (Map.toList . fmap Set.size)  _finsPendingMessages)

class FinalizationQueueLenses s => FinalizationStateLenses s timer | s -> timer where
    finState :: Lens' s (FinalizationState timer)
    finSessionId :: Lens' s FinalizationSessionId
    finSessionId = finState . finsSessionId
    finIndex :: Lens' s FinalizationIndex
    finIndex = finState . finsIndex
    finHeight :: Lens' s BlockHeight
    finHeight = finState . finsHeight
    -- |The round delta for the starting round at the current finalization index.
    finIndexInitialDelta :: Lens' s BlockHeight
    finIndexInitialDelta = finState . finsIndexInitialDelta
    finCommittee :: Lens' s FinalizationCommittee
    finCommittee = finState . finsCommittee
    -- |The minimum distance between finalized blocks will be @1 + finMinSkip@.
    finMinSkip :: Lens' s BlockHeight
    finMinSkip = finState . finsMinSkip
    -- |All received finalization messages for the current and future finalization indexes.
    -- (Previously, this was just future messages, but now we store all of them for catch-up purposes.)
    finPendingMessages :: Lens' s PendingMessageMap
    finPendingMessages = finState . finsPendingMessages
    finCurrentRound :: Lens' s (Either PassiveFinalizationRound FinalizationRound)
    finCurrentRound = finState . finsCurrentRound
    -- |For each failed round (from most recent to oldest), signatures
    -- on @WeAreDone False@ proving failure.
    finFailedRounds :: Lens' s [Map Party Sig.Signature]
    finFailedRounds = finState . finsFailedRounds
    finCatchUpTimer :: Lens' s (Maybe timer)
    finCatchUpTimer = finState . finsCatchUpTimer
    finCatchUpAttempts :: Lens' s Int
    finCatchUpAttempts = finState . finsCatchUpAttempts
    finCatchUpDeDup :: Lens' s (PSQ.OrdPSQ Sig.Signature UTCTime ())
    finCatchUpDeDup = finState . finsCatchUpDeDup

instance FinalizationQueueLenses (FinalizationState m) where
    finQueue = finsQueue

instance FinalizationStateLenses (FinalizationState m) m where
    finState = id

initialPassiveFinalizationState :: BlockHash -> FinalizationParameters -> Bakers -> Amount -> FinalizationState timer
initialPassiveFinalizationState genHash finParams genBakers totalGTU = FinalizationState {
    _finsSessionId = FinalizationSessionId genHash 0,
    _finsIndex = 1,
    _finsHeight = 1 + finalizationMinimumSkip finParams,
    _finsIndexInitialDelta = 1,
    _finsCommittee = makeFinalizationCommittee finParams totalGTU genBakers,
    _finsMinSkip = finalizationMinimumSkip finParams,
    _finsPendingMessages = Map.empty,
    _finsCurrentRound = Left initialPassiveFinalizationRound,
    _finsFailedRounds = [],
    _finsCatchUpTimer = Nothing,
    _finsCatchUpAttempts = 0,
    _finsCatchUpDeDup = PSQ.empty,
    _finsQueue = initialFinalizationQueue
    }
{-# INLINE initialPassiveFinalizationState #-}

initialFinalizationState :: FinalizationInstance -> BlockHash -> FinalizationParameters -> Bakers -> Amount -> FinalizationState timer
initialFinalizationState FinalizationInstance{..} genHash finParams genBakers totalGTU = (initialPassiveFinalizationState genHash finParams genBakers totalGTU) {
    _finsCurrentRound = case filter (\p -> partySignKey p == Sig.verifyKey finMySignKey && partyVRFKey p == VRF.publicKey finMyVRFKey) (Vec.toList (parties com)) of
        [] -> Left initialPassiveFinalizationRound
        (p:_) -> Right FinalizationRound {
            roundInput = Nothing,
            roundDelta = 1,
            roundMe = partyIndex p,
            roundWMVBA = initialWMVBAState
        }
    }
    where
        com = makeFinalizationCommittee finParams totalGTU genBakers

getFinalizationInstance :: (MonadReader r m, HasFinalizationInstance r) => m (Maybe FinalizationInstance)
getFinalizationInstance = asks finalizationInstance

type FinalizationStateMonad r s m = (MonadState s m, FinalizationStateLenses s (Timer m), MonadReader r m, HasFinalizationInstance r)

type FinalizationBaseMonad r s m = (BlockPointerMonad m, TreeStateMonad m, SkovMonad m, FinalizationStateMonad r s m, MonadIO m, TimerMonad m, FinalizationOutputMonad m)

-- |This sets the base time for triggering finalization replay.
finalizationReplayBaseDelay :: NominalDiffTime
finalizationReplayBaseDelay = 300

-- |This sets the per-party additional delay for finalization replay.
-- 
finalizationReplayStaggerDelay :: NominalDiffTime
finalizationReplayStaggerDelay = 5

-- |Reset the finalization catch-up timer.  This is called when progress is
-- made in finalization (i.e. we produce a message).
doResetTimer :: (FinalizationBaseMonad r s m) => m ()
doResetTimer = do
        oldTimer <- finCatchUpTimer <<.= Nothing
        forM_ oldTimer cancelTimer
        curRound <- use finCurrentRound
        forM_ curRound $ \FinalizationRound{..} ->
            let spawnTimer = do
                    attempts <- use finCatchUpAttempts
                    logEvent Afgjort LLTrace $ "Setting replay timer (attempts: " ++ show attempts ++ ")"
                    timer <- onTimeout (DelayFor $ fromIntegral (attempts + 1) * (finalizationReplayBaseDelay + finalizationReplayStaggerDelay * fromIntegral roundMe)) $
                        getFinalizationInstance >>= mapM_ (\finInst -> do
                            finSt <- get
                            logEvent Afgjort LLTrace $ "Sending finalization summary (attempt " ++ show (attempts + 1) ++ ")"
                            mapM_ broadcastFinalizationPseudoMessage (finalizationCatchUpMessage finInst finSt)
                            finCatchUpAttempts %= (+1)
                            spawnTimer)
                    finCatchUpTimer ?= timer
            in spawnTimer

tryNominateBlock :: (FinalizationBaseMonad r s m, FinalizationMonad m) => m ()
tryNominateBlock = do
    curRound <- use finCurrentRound
    forM_ curRound $ \r@FinalizationRound{..} ->
        when (isNothing roundInput) $ do
            h <- use finHeight
            bBlock <- bestBlock
            when (bpHeight bBlock >= h + roundDelta) $ do
                ancestor <- ancestorAtHeight h bBlock
                let nomBlock = bpHash ancestor
                finCurrentRound .= Right (r {roundInput = Just nomBlock})
                simpleWMVBA $ startWMVBA nomBlock

nextRound :: (FinalizationBaseMonad r s m, FinalizationMonad m) => FinalizationIndex -> BlockHeight -> m ()
nextRound oldFinIndex oldDelta = do
    curFinIndex <- use finIndex
    when (curFinIndex == oldFinIndex) $ do
        oldRound <- use finCurrentRound
        forM_ oldRound $ \r ->
            when (roundDelta r == oldDelta) $ do
                finFailedRounds %= (wmvbaWADBot (roundWMVBA r) :)
                newRound (2 * oldDelta) (roundMe r)

pendingToFinMsg :: FinalizationSessionId -> FinalizationIndex -> BlockHeight -> PendingMessage -> FinalizationMessage
pendingToFinMsg sessId finIx delta (PendingMessage src msg sig) =
     let msgHdr party = FinalizationMessageHeader {
             msgSessionId = sessId,
             msgFinalizationIndex = finIx,
             msgDelta = delta,
             msgSenderIndex = party
         }
     in FinalizationMessage (msgHdr src) msg sig

newRound :: (FinalizationBaseMonad r s m, FinalizationMonad m) => BlockHeight -> Party -> m ()
newRound newDelta me = do
        finCurrentRound .= Right FinalizationRound {
            roundInput = Nothing,
            roundDelta = newDelta,
            roundMe = me,
            roundWMVBA = initialWMVBAState
        }
        h <- use finHeight
        logEvent Afgjort LLDebug $ "Starting finalization round: height=" ++ show (theBlockHeight h) ++ " delta=" ++ show (theBlockHeight newDelta)
        blocksAtHeight <- getBlocksAtHeight (h + newDelta)
        justifiedInputs <- mapM (ancestorAtHeight h) blocksAtHeight
        finIx <- use finIndex
        committee <- use finCommittee
        sessId <- use finSessionId
        -- Filter the messages that have valid signatures and reference legitimate parties
        -- TODO: Drop pending messages for this round, because we've handled them
        let toFinMsg = pendingToFinMsg sessId finIx newDelta
        pmsgs <- finPendingMessages . atStrict finIx . non Map.empty . atStrict newDelta . non Set.empty <%= Set.filter (checkMessage committee . toFinMsg)
        -- Justify the blocks
        forM_ justifiedInputs $ \i -> do
            logEvent Afgjort LLTrace $ "Justified input at " ++ show finIx ++ ": " ++ show i
            simpleWMVBA $ justifyWMVBAInput $ bpHash i
        -- Receive the pending messages
        forM_ pmsgs $ \smsg@(PendingMessage src msg sig) -> do
            logEvent Afgjort LLDebug $ "Handling message: " ++ show (toFinMsg smsg)
            simpleWMVBA $ receiveWMVBAMessage src sig msg
        tryNominateBlock

-- TODO (MR) If this code is correct, consider reducing duplication with `receiveFinalizationMessage`
newPassiveRound :: (FinalizationBaseMonad r s m, FinalizationMonad m) => BlockHeight -> BlockPointerType m -> m ()
newPassiveRound newDelta bp = do
    nextFinHeight <- nextFinalizationHeight newDelta bp
    finCom        <- use finCommittee
    finInd        <- use finIndex
    sessionId     <- use finSessionId
    maybeWitnessMsgs <- finPendingMessages . atStrict finInd . non Map.empty 
                                           . atStrict nextFinHeight . non Set.empty 
                                           <%= Set.filter (checkMessage finCom . pendingToFinMsg sessionId finInd newDelta)
    let finParties = parties finCom
        partyInfo party = finParties Vec.! fromIntegral party
        pWeight = partyWeight . partyInfo
        pVRFKey = partyVRFKey . partyInfo
        pBlsKey = partyBlsKey . partyInfo
        baid = roundBaid sessionId finInd newDelta
        maxParty = fromIntegral $ Vec.length finParties - 1
        inst = WMVBAInstance baid (totalWeight finCom) (corruptWeight finCom) pWeight maxParty pVRFKey undefined undefined pBlsKey undefined
    forM_ maybeWitnessMsgs $ \(PendingMessage src msg _) -> do
        let (mProof, _) = runState (passiveReceiveWMVBAMessage inst src msg) initialWMVBAPassiveState
        forM_ mProof (handleFinalizationProof sessionId finInd newDelta finCom)
    finCurrentRound .= Left initialPassiveFinalizationRound

handleWMVBAOutputEvents :: (FinalizationBaseMonad r s m, FinalizationMonad m) => FinalizationInstance -> [WMVBAOutputEvent Sig.Signature] -> m ()
handleWMVBAOutputEvents FinalizationInstance{..} evs = do
        FinalizationState{..} <- use finState
        forM_ _finsCurrentRound $ \FinalizationRound{..} -> do
            let msgHdr = FinalizationMessageHeader{
                msgSessionId = _finsSessionId,
                msgFinalizationIndex = _finsIndex,
                msgDelta = roundDelta,
                msgSenderIndex = roundMe
            }
            let
                handleEvs _ [] = return ()
                handleEvs b (SendWMVBAMessage msg0 : evs') = do
                    case msg0 of
                        WMVBAFreezeMessage (Proposal v) -> logEvent Afgjort LLDebug $ "Nominating block " ++ show v
                        _ -> return ()
                    let msg = signFinalizationMessage finMySignKey msgHdr msg0
                    broadcastFinalizationMessage msg
                    -- We manually loop back messages here
                    _ <- receiveFinalizationMessage msg
                    finCatchUpAttempts .= 0
                    doResetTimer
                    handleEvs b evs'
                handleEvs False (WMVBAComplete Nothing : evs') = do
                    -- Round failed, so start a new one
                    nextRound _finsIndex roundDelta
                    handleEvs True evs'
                handleEvs False (WMVBAComplete (Just proof) : evs') = do
                    -- Round completed, so handle the proof.
                    handleFinalizationProof _finsSessionId _finsIndex roundDelta _finsCommittee proof
                    handleEvs True evs'
                handleEvs True (WMVBAComplete _ : evs') = handleEvs True evs'
            handleEvs False evs

-- |Handle when a finalization proof is generated:
--  * Notify Skov of finalization ('trustedFinalize').
--  * If the finalized block is known to Skov, handle this new finalization ('finalizationBlockFinal').
--  * If the block is not known, add the finalization to the queue ('addQueuedFinalization').
handleFinalizationProof :: (FinalizationMonad m, SkovMonad m, MonadState s m, FinalizationQueueLenses s) => FinalizationSessionId -> FinalizationIndex -> BlockHeight -> FinalizationCommittee -> (Val, ([Party], Bls.Signature)) -> m ()
handleFinalizationProof sessId fIndex delta committee (finB, (parties, sig)) = do
        let finRec = FinalizationRecord {
            finalizationIndex = fIndex,
            finalizationBlockPointer = finB,
            finalizationProof = FinalizationProof (parties, sig),
            finalizationDelay = delta
        }
        finRes <- trustedFinalize finRec
        case finRes of
            Left _ -> addQueuedFinalization sessId committee finRec
            Right finBlock -> finalizationBlockFinal finRec finBlock


liftWMVBA :: (FinalizationBaseMonad r s m, FinalizationMonad m) => FinalizationInstance -> WMVBA Sig.Signature a -> m a
liftWMVBA fininst@FinalizationInstance{..} a = do
    FinalizationState{..} <- use finState
    case _finsCurrentRound of
        Left _ -> error "No current finalization round"
        Right fr@FinalizationRound{..} -> do
            let
                baid = roundBaid _finsSessionId _finsIndex roundDelta
                pWeight party = partyWeight (parties _finsCommittee Vec.! fromIntegral party)
                pVRFKey party = partyVRFKey (parties _finsCommittee Vec.! fromIntegral party)
                pBlsKey party = partyBlsKey (parties _finsCommittee Vec.! fromIntegral party)
                maxParty = fromIntegral $ Vec.length (parties _finsCommittee) - 1
                inst = WMVBAInstance baid (totalWeight _finsCommittee) (corruptWeight _finsCommittee) pWeight maxParty pVRFKey roundMe finMyVRFKey pBlsKey finMyBlsKey
            (r, newState, evs) <- liftIO $ runWMVBA a inst roundWMVBA
            finCurrentRound .= Right fr {roundWMVBA = newState}
            -- logEvent Afgjort LLTrace $ "New WMVBA state: " ++ show newState
            handleWMVBAOutputEvents fininst evs
            return r

simpleWMVBA :: (FinalizationBaseMonad r s m, FinalizationMonad m) => WMVBA Sig.Signature () -> m ()
simpleWMVBA a = getFinalizationInstance >>= \case
    Just inst -> liftWMVBA inst a
    Nothing -> logEvent Afgjort LLError $ "Finalization keys missing, but this node appears to be participating in finalization."

-- |Determine if a message references blocks requiring Skov to catch up.
messageRequiresCatchUp :: (FinalizationBaseMonad r s m, FinalizationMonad m) => WMVBAMessage -> m Bool
messageRequiresCatchUp msg = case messageValues msg of
        Nothing -> return False
        Just b -> resolveBlock b >>= \case
            Nothing -> return True -- Block not found
            Just _ -> do
                FinalizationState{..} <- use finState
                minst <- getFinalizationInstance
                case (_finsCurrentRound, minst) of
                    -- Check that the block is considered justified.
                    (Right _, Just finInst) -> liftWMVBA finInst $ isJustifiedWMVBAInput b
                    -- TODO: possibly we should also check if it is justified even when we are not active in finalization
                    _ -> return False

savePendingMessage :: (FinalizationBaseMonad r s m) => FinalizationIndex -> BlockHeight -> PendingMessage -> m Bool
savePendingMessage finIx finDelta pmsg = do
    pmsgs <- use finPendingMessages
    case Map.lookup finIx pmsgs of
        Nothing -> do
            finPendingMessages .= Map.insert finIx (Map.singleton finDelta $ Set.singleton pmsg) pmsgs
            return False
        Just ipmsgs -> case Map.lookup finDelta ipmsgs of
            Nothing -> do
                finPendingMessages .= Map.insert finIx (Map.insert finDelta (Set.singleton pmsg) ipmsgs) pmsgs
                return False
            Just s -> if pmsg `Set.member` s then
                    return True
                else do
                    finPendingMessages .= Map.insert finIx (Map.insert finDelta (Set.insert pmsg s) ipmsgs) pmsgs
                    return False

-- |Called when a finalization message is received.
receiveFinalizationMessage :: (FinalizationBaseMonad r s m, FinalizationMonad m) => FinalizationMessage -> m UpdateResult
receiveFinalizationMessage msg@FinalizationMessage{msgHeader=FinalizationMessageHeader{..},..} = do
        FinalizationState{..} <- use finState
        -- Check this is the right session
        if _finsSessionId == msgSessionId then
            -- Check the finalization index is not out of date
            case compare msgFinalizationIndex _finsIndex of
                LT -> tryAddQueuedWitness msg
                GT -> -- Message is from the future; consider it invalid if it's not the index after the current one.
                    if msgFinalizationIndex - _finsIndex < 2 then do
                        -- Save the message for a later finalization index
                        isDuplicate <- savePendingMessage msgFinalizationIndex msgDelta (PendingMessage msgSenderIndex msgBody msgSignature)
                        if isDuplicate then
                            return ResultDuplicate
                        else do
                            -- Since we're behind, request the finalization record we're apparently missing
                            logEvent Afgjort LLDebug $ "Missing finalization at index " ++ show (msgFinalizationIndex - 1)
                            return ResultPendingFinalization
                    else
                        return ResultInvalid -- FIXME: possibly return ResultUnverifiable instead.
                EQ -> -- handle the message now, since it's the current round
                    if checkMessage _finsCommittee msg then do
                        -- Save the message
                        isDuplicate <- savePendingMessage msgFinalizationIndex msgDelta (PendingMessage msgSenderIndex msgBody msgSignature)
                        if isDuplicate then
                            return ResultDuplicate
                        else do
                            -- Check if we're participating in finalization for this index
                            case _finsCurrentRound of
                                Right (FinalizationRound{..}) ->
                                    -- And it's the current round
                                    when (msgDelta == roundDelta) $ do
                                        logEvent Afgjort LLDebug $ "Handling message: " ++ show msg
                                        simpleWMVBA (receiveWMVBAMessage msgSenderIndex msgSignature msgBody)
                                Left (PassiveFinalizationRound pw) -> do
                                    let
                                        baid = roundBaid _finsSessionId _finsIndex msgDelta
                                        pWeight party = partyWeight (parties _finsCommittee Vec.! fromIntegral party)
                                        pVRFKey party = partyVRFKey (parties _finsCommittee Vec.! fromIntegral party)
                                        pBlsKey party = partyBlsKey (parties _finsCommittee Vec.! fromIntegral party)
                                        maxParty = fromIntegral $ Vec.length (parties _finsCommittee) - 1
                                        inst = WMVBAInstance baid (totalWeight _finsCommittee) (corruptWeight _finsCommittee) pWeight maxParty pVRFKey undefined undefined pBlsKey undefined
                                        (mProof, ps') = runState (passiveReceiveWMVBAMessage inst msgSenderIndex msgBody) (pw ^. atStrict msgDelta . non initialWMVBAPassiveState)
                                    finCurrentRound .= Left (PassiveFinalizationRound (pw & atStrict msgDelta ?~ ps'))
                                    forM_ mProof (handleFinalizationProof _finsSessionId _finsIndex msgDelta _finsCommittee)
                            rcu <- messageRequiresCatchUp msgBody
                            if rcu then do
                                logEvent Afgjort LLDebug $ "Message refers to unjustified block; catch-up required."
                                return ResultPendingBlock
                            else
                                return ResultSuccess
                    else do
                        logEvent Afgjort LLWarning $ "Received bad finalization message"
                        return ResultInvalid
            else
                return ResultIncorrectFinalizationSession

-- |Called when a finalization pseudo-message is received.
receiveFinalizationPseudoMessage :: (FinalizationBaseMonad r s m, FinalizationMonad m) => FinalizationPseudoMessage -> m UpdateResult
receiveFinalizationPseudoMessage (FPMMessage msg) = receiveFinalizationMessage msg
receiveFinalizationPseudoMessage (FPMCatchUp cu@CatchUpMessage{..}) = do
        FinalizationState{..} <- use finState
        if _finsSessionId == cuSessionId then
            case compare cuFinalizationIndex _finsIndex of
                LT -> return ResultStale
                GT -> return ResultUnverifiable
                EQ -> if checkCatchUpMessageSignature _finsCommittee cu then do
                        now <- currentTime
                        oldDeDup <- use finCatchUpDeDup
                        let
                            (_, purgedDeDup) = PSQ.atMostView (addUTCTime (-60) now) oldDeDup
                            alterfun Nothing = (False, Just (now, ()))
                            alterfun (Just _) = (True, Just (now, ()))
                            (isDup, newDeDup) = PSQ.alter alterfun cuSignature purgedDeDup
                        finCatchUpDeDup .= newDeDup
                        if isDup then
                            return ResultDuplicate
                        else do
                            logEvent Afgjort LLTrace $ "Processing finalization summary from " ++ show cuSenderIndex
                            CatchUpResult{..} <- processFinalizationSummary cuFinalizationSummary
                            logEvent Afgjort LLTrace $ "Finalization summary was " ++ (if curBehind then "behind" else "not behind")
                                        ++ " and " ++ (if curSkovCatchUp then "requires Skov catch-up." else "does not require Skov catch-up.")
                            unless curBehind doResetTimer
                            if curSkovCatchUp then
                                return ResultPendingBlock
                            else
                                return ResultSuccess
                    else
                        return ResultInvalid
        else
            return ResultIncorrectFinalizationSession

-- |Handle receipt of a finalization record.
--
-- If the record is for a finalization index that is settled (i.e. the finalization
-- record appears in a finalized block) then this returns 'ResultStale'.
--
-- If the record is for a finalization index where a valid finalization record is already
-- known, then one of the following applies:
--
--   * If the record is invalid, returns 'ResultInvalid'.
--   * If the record is valid and contains new signatures, stores the record and returns 'ResultSuccess'.
--   * If @validateDuplicate@ is not set or the record is valid, returns 'ResultDuplicate'.
--
-- When more than one case could apply, it is unspecified which is chosen. It is intended that
-- 'ResultSuccess' should be used wherever possible, but 'ResultDuplicate' can be returned in any
-- case.
--
-- If the record is for the next finalization index:
--
--   * If the record is valid and for a known block, that block is finalized and 'ResultSuccess' returned.
--   * If the record is invalid, 'ResultInvalid' is returned.
--   * If the block is unknown, then 'ResultUnverifiable' is returned.
--
-- If the record is for a future finalization index (that is not next), 'ResultUnverifiable' is returned
-- and the record is discarded.
receiveFinalizationRecord :: (SkovMonad m, MonadState s m, FinalizationQueueLenses s, FinalizationMonad m) => Bool -> FinalizationRecord -> m UpdateResult
receiveFinalizationRecord validateDuplicate finRec@FinalizationRecord{..} = do
        nextFinIx <- nextFinalizationIndex
        case compare finalizationIndex nextFinIx of
            LT -> do
                fi <- use (finQueue . fqFirstIndex)
                if finalizationIndex < fi then
                    return ResultStale
                else if validateDuplicate then
                    checkFinalizationProof finRec >>= \case
                        Nothing -> return ResultInvalid
                        Just (finSessId, finCom) -> do
                            addQueuedFinalization finSessId finCom finRec
                            return ResultDuplicate
                else
                    return ResultDuplicate
            EQ -> checkFinalizationProof finRec >>= \case
                Nothing -> return ResultInvalid
                Just _ -> trustedFinalize finRec >>= \case
                    -- In this case, we have received a valid finalization proof,
                    -- but it's not for a block that is known.  This shouldn't happen
                    -- often, and we are probably fine to throw it away.
                    Left res -> return res
                    Right newFinBlock -> do
                        -- finalizationBlockFinal adds the finalization to the queue
                        finalizationBlockFinal finRec newFinBlock
                        return ResultSuccess
            GT -> return ResultUnverifiable

-- |It is possible to have a validated finalization proof for a block that is
-- not currently known.  This function detects when such a block arrives and
-- triggers it to be finalized.
notifyBlockArrivalForPending :: (SkovMonad m, MonadState s m, FinalizationQueueLenses s, BlockPointerData bp, FinalizationMonad m) => bp -> m ()
notifyBlockArrivalForPending b = do
    nfi <- nextFinalizationIndex
    getQueuedFinalizationTrivial nfi >>= \case
        Just finRec
            | finalizationBlockPointer finRec == bpHash b ->
                trustedFinalize finRec >>= \case
                    Right newFinBlock -> finalizationBlockFinal finRec newFinBlock
                    Left _ -> return ()
        _ -> return ()

-- |Called to notify the finalization routine when a new block arrives.
notifyBlockArrival :: (FinalizationBaseMonad r s m, FinalizationMonad m) => BlockPointerType m -> m ()
notifyBlockArrival b = do
    notifyBlockArrivalForPending b
    FinalizationState{..} <- use finState
    forM_ _finsCurrentRound $ \FinalizationRound{..} -> do
        when (bpHeight b == _finsHeight + roundDelta) $ do
            ancestor <- ancestorAtHeight _finsHeight b
            logEvent Afgjort LLTrace $ "Justified input at " ++ show _finsIndex ++ ": " ++ show (bpHash ancestor)
            simpleWMVBA $ justifyWMVBAInput (bpHash ancestor)
        tryNominateBlock

-- |Determine what index we have in the finalization committee.
-- This simply finds the first party in the committee whose
-- public keys match ours.
getMyParty :: (FinalizationBaseMonad r s m) => m (Maybe Party)
getMyParty = getFinalizationInstance >>= \case
    Nothing -> return Nothing
    Just finInst -> do
        let
            myVerifyKey = (Sig.verifyKey . finMySignKey) finInst
            myPublicVRFKey = (VRF.publicKey . finMyVRFKey) finInst
        ps <- parties <$> use finCommittee
        case filter (\p -> partySignKey p == myVerifyKey && partyVRFKey p == myPublicVRFKey) (Vec.toList ps) of
            (p:_) -> return $ Just (partyIndex p)
            [] -> return Nothing

-- |Produce 'OutputWitnesses' based on the pending finalization messages.
-- This is used when we know finalization has occurred (by receiving a
-- valid finalization record) but we have not completed finalization, and
-- in particular, have not yet reached the round in which finalization
-- completed.  (In the case where we have reached that round,
-- 'getOutputWitnesses' should be called on the WMVBA instance for that round
-- instead.)
pendingToOutputWitnesses :: (FinalizationBaseMonad r s m)
    => FinalizationSessionId
    -> FinalizationIndex
    -> BlockHeight
    -> BlockHash
    -> m OutputWitnesses
pendingToOutputWitnesses sessId finIx delta finBlock = do
        -- Get the pending messages at the given finalization index and delta.
        pmsgs <- use $ finPendingMessages . atStrict finIx . non Map.empty . atStrict delta . non Set.empty
        committee <- use finCommittee
        -- Filter for only the witness creator messages that witness the correct
        -- block and are correctly signed.
        let
            f (PendingMessage src msg@(WMVBAWitnessCreatorMessage (b,blssig)) sig)
                | b == finBlock
                , checkMessage committee (FinalizationMessage (msgHdr src) msg sig)
                    = Just (src, blssig)
            f _ = Nothing
            msgHdr src = FinalizationMessageHeader {
                msgSessionId = sessId,
                msgFinalizationIndex = finIx,
                msgDelta = delta,
                msgSenderIndex = src
            }
            filtpmsgs = mapMaybe f (Set.toList pmsgs)
        -- The returned OutputWitnesses only consists of unchecked signatures,
        -- since we have made no effort to check the BLS signatures.
        return $ uncheckedOutputWitnesses (Map.fromList filtpmsgs)

-- |Called to notify the finalization routine when a new block is finalized.
-- (NB: this should never be called with the genesis block.)
notifyBlockFinalized :: (FinalizationBaseMonad r s m, FinalizationMonad m) => FinalizationRecord -> BlockPointerType m -> m ()
notifyBlockFinalized fr@FinalizationRecord{..} bp = do
        -- Reset catch-up timer
        oldTimer <- finCatchUpTimer <<.= Nothing
        forM_ oldTimer cancelTimer
        finCatchUpAttempts .= 0
        -- Reset the deduplication buffer
        finCatchUpDeDup .= PSQ.empty
        -- Move to next index
        oldFinIndex <- finIndex <<.= finalizationIndex + 1
        unless (finalizationIndex == oldFinIndex) $ error "Non-sequential finalization"
        -- Update the finalization queue index as necessary
        getBlockStatus (bpLastFinalizedHash bp) >>= \case
            Just (BlockFinalized _ FinalizationRecord{finalizationIndex = fi}) -> updateQueuedFinalizationIndex (fi + 1)
            _ -> error "Invariant violation: notifyBlockFinalized called on block with last finalized block that is not finalized."
        -- Add all witnesses we have to the finalization queue
        sessId <- use finSessionId
        fc <- use finCommittee
        witnesses <- use finCurrentRound >>= \case
            -- If we aren't participating in this finalization round, we get the witnesses
            -- from the passive state.
            Left (PassiveFinalizationRound{..}) ->
                return $ passiveGetOutputWitnesses
                            finalizationBlockPointer
                            (passiveWitnesses ^. atStrict finalizationDelay . non initialWMVBAPassiveState)
            Right curRound
                | roundDelta curRound == finalizationDelay ->
                    -- If the WMVBA is on the same round as the finalization proof, get
                    -- the additional witnesses from there.
                    return $ getOutputWitnesses finalizationBlockPointer (roundWMVBA curRound)
                | otherwise ->
                    -- If not, get the witnesses from the pending queue.
                    pendingToOutputWitnesses sessId finalizationIndex finalizationDelay finalizationBlockPointer
        addNewQueuedFinalization sessId fc fr witnesses
        -- Discard finalization messages from old round
        finPendingMessages . atStrict finalizationIndex .= Nothing
        pms <- use finPendingMessages
        logEvent Afgjort LLTrace $ "Finalization complete. Pending messages: " ++ show pms
        let newFinDelay = nextFinalizationDelay fr
        fs <- use finMinSkip
        nfh <- nextFinalizationHeight fs bp
        finHeight .= nfh
        finIndexInitialDelta .= newFinDelay
        finFailedRounds .= []
        -- Update finalization committee for the new round
        finCommittee <~ getFinalizationCommittee bp
        -- Determine if we're in the committee
        mMyParty <- getMyParty
        case mMyParty of
          Just myParty ->
            newRound newFinDelay myParty
          Nothing ->
            newPassiveRound newFinDelay bp

nextFinalizationDelay :: FinalizationRecord -> BlockHeight
nextFinalizationDelay FinalizationRecord{..} = if finalizationDelay > 2 then finalizationDelay `div` 2 else 1

-- |Given the finalization minimum skip and an explicitly finalized block, compute
-- the height of the next finalized block.
nextFinalizationHeight :: (BlockPointerMonad m)
    => BlockHeight -- ^Finalization minimum skip
    -> BlockPointerType m -- ^Last finalized block
    -> m BlockHeight
nextFinalizationHeight fs bp = do
  lf <- bpLastFinalized bp
  return $ bpHeight bp + max (1 + fs) ((bpHeight bp - bpHeight lf) `div` 2)

-- |The height that a chain must be for a block to be eligible for finalization.
-- This is the next finalization height + the next finalization delay.
nextFinalizationJustifierHeight :: (BlockPointerMonad m)
    => FinalizationParameters
    -> FinalizationRecord -- ^Last finalization record
    -> BlockPointerType m -- ^Last finalized block
    -> m BlockHeight
nextFinalizationJustifierHeight fp fr bp = (+ nextFinalizationDelay fr) <$> nextFinalizationHeight (finalizationMinimumSkip fp) bp

getPartyWeight :: FinalizationCommittee -> Party -> VoterPower
getPartyWeight com pid = case parties com ^? ix (fromIntegral pid) of
        Nothing -> 0
        Just p -> partyWeight p

-- |Check that a finalization record has a valid proof
verifyFinalProof :: FinalizationSessionId -> FinalizationCommittee -> FinalizationRecord -> Bool
verifyFinalProof sid com@FinalizationCommittee{..} FinalizationRecord{..} =
        sigWeight finParties > corruptWeight && checkProofSignature
    where
        FinalizationProof (finParties, sig) = finalizationProof
        toSign = witnessMessage (roundBaid sid finalizationIndex finalizationDelay) finalizationBlockPointer
        mpks = sequence ((fmap partyBlsKey . toPartyInfo com) <$> finParties)
        checkProofSignature = case mpks of
            Nothing -> False -- If any parties are invalid, reject the proof
            Just pks -> Bls.verifyAggregate toSign pks sig
        sigWeight ps = sum (getPartyWeight com <$> ps)

-- |Check a finalization proof, returning the session id and finalization committee if
-- successful.
checkFinalizationProof :: (SkovQueryMonad m) => FinalizationRecord -> m (Maybe (FinalizationSessionId, FinalizationCommittee))
checkFinalizationProof finRec = getFinalizationContext finRec <&> \case
        Nothing -> Nothing
        Just (finSessId, finCom) -> if verifyFinalProof finSessId finCom finRec then Just (finSessId, finCom) else Nothing

-- |Produce a 'FinalizationSummary' based on the finalization state.
finalizationSummary :: (FinalizationStateLenses s m) => SimpleGetter s FinalizationSummary
finalizationSummary = to fs
    where
        fs s = FinalizationSummary{..}
            where
                summaryFailedRounds = reverse $ s ^. finFailedRounds
                summaryCurrentRound = case s ^. finCurrentRound of
                    Left _ -> WMVBASummary Nothing Nothing Nothing
                    Right FinalizationRound{..} -> roundWMVBA ^. wmvbaSummary

-- |Produce a 'FinalizationPseudoMessage' containing a catch up message based on the current finalization state.
finalizationCatchUpMessage :: (FinalizationStateLenses s m) => FinalizationInstance -> s -> Maybe FinalizationPseudoMessage
finalizationCatchUpMessage FinalizationInstance{..} s = either (const Nothing) Just _finsCurrentRound <&> \FinalizationRound{..} ->
        FPMCatchUp $! signCatchUpMessage finMySignKey _finsSessionId _finsIndex roundMe (committeeMaxParty _finsCommittee) summary
    where
        FinalizationState{..} = s ^. finState
        summary = s ^. finalizationSummary

-- |Process a 'FinalizationSummary', handling any new messages and returning a result indicating
-- whether the summary is behind, and whether we should initiate Skov catch-up.
processFinalizationSummary :: (FinalizationBaseMonad r s m, FinalizationMonad m) => FinalizationSummary -> m CatchUpResult
processFinalizationSummary FinalizationSummary{..} =
        use finCurrentRound >>= \case
            Left _ -> return mempty -- TODO: actually do something with these
            Right _ -> getFinalizationInstance >>= \case
                Nothing -> return mempty -- This should not happen, since it means that we seem to be participating in finalization
                                            -- but do not have keys to do so
                Just finInst@(FinalizationInstance{..}) -> do
                    committee@FinalizationCommittee{..} <- use finCommittee
                    initDelta <- use finIndexInitialDelta
                    msgSessionId <- use finSessionId
                    msgFinalizationIndex <- use finIndex
                    let
                        mkFinalizationMessage :: BlockHeight -> Party -> WMVBAMessage -> Sig.Signature -> FinalizationMessage
                        mkFinalizationMessage msgDelta msgSenderIndex = FinalizationMessage FinalizationMessageHeader{..}
                        checkSigDelta :: BlockHeight -> Party -> WMVBAMessage -> Sig.Signature -> Bool
                        checkSigDelta msgDelta msgSenderIndex msg sig = checkMessageSignature committee (mkFinalizationMessage msgDelta msgSenderIndex msg sig)
                    roundsBehind <- forM (zip [0..] summaryFailedRounds) $
                        \(roundIndex, m) -> let delta = BlockHeight (shiftL (theBlockHeight initDelta) roundIndex) in use finCurrentRound >>= \case
                        -- Note, we need to get the current round each time, because processing might advance the round
                        Left _ -> return False
                        Right curRound -> case compare delta (roundDelta curRound) of
                                LT -> do
                                    -- The round should already be failed for us
                                    -- Just check the signatures to see if it is behind.
                                    let
                                        -- TODO: Use existing signatures to short-cut signature checking
                                        checkSig party sig = checkSigDelta delta party wmvbaWADBotMessage sig
                                        cur' = Map.filterWithKey checkSig m
                                    -- We consider it behind if it doesn't include (n-t) valid signatures
                                    return $ sum (getPartyWeight committee <$> Map.keys cur') < totalWeight - corruptWeight
                                EQ -> -- This is our current round, so create a WMVBASummary and process that
                                    curBehind <$> liftWMVBA finInst (processWMVBASummary (wmvbaFailedSummary m) (checkSigDelta delta))
                                GT -> -- This case shouldn't happen unless the message is corrupt.
                                    return False
                    let delta = BlockHeight (shiftL (theBlockHeight initDelta) (length summaryFailedRounds))
                    use finCurrentRound >>= \case
                        Left _ -> return mempty
                        Right curRound -> case compare delta (roundDelta curRound) of
                            LT -> return (CatchUpResult {curBehind = True, curSkovCatchUp = False})
                            EQ -> do
                                cur <- liftWMVBA finInst $ processWMVBASummary summaryCurrentRound (checkSigDelta delta)
                                return (cur <> mempty {curBehind = or roundsBehind})
                            GT -> return (mempty {curBehind = or roundsBehind})


-- |Given an existing block, returns a 'FinalizationRecord' that can be included in
-- a child of that block, if available.
nextFinalizationRecord :: (FinalizationMonad m, SkovMonad m) => BlockPointerType m -> m (Maybe FinalizationRecord)
nextFinalizationRecord parentBlock = do
    lfi <- blockLastFinalizedIndex parentBlock
    finalizationUnsettledRecordAt (lfi + 1)

-- |'ActiveFinalizationM' provides an implementation of 'FinalizationMonad' that
-- actively participates in finalization.
newtype ActiveFinalizationM r s m a = ActiveFinalizationM {runActiveFinalizationM :: m a}
    deriving (Functor, Applicative, Monad, MonadState s, MonadReader r, TimerMonad, BlockStateTypes, BlockStateQuery, BlockStateOperations, BlockStateStorage, BlockPointerMonad, PerAccountDBOperations, TreeStateMonad, SkovQueryMonad, SkovMonad, TimeMonad, LoggerMonad, MonadIO, FinalizationOutputMonad)

deriving instance (BlockPointerData (BlockPointerType m), BlockPendingData (PendingBlockType m)) => GlobalStateTypes (ActiveFinalizationM r s m)
deriving instance (CanExtend (ATIStorage m), CanRecordFootprint (Footprint (ATIStorage m))) => ATITypes (ActiveFinalizationM r s m)
-- deriving instance (Convert a b m) => Convert a b (ActiveFinalizationM r s m)

{-
instance GlobalStateTypes m => GlobalStateTypes (ActiveFinalizationM r s m) where
    type PendingBlock (ActiveFinalizationM r s m) = PendingBlock m
    type BlockPointer (ActiveFinalizationM r s m) = BlockPointer m
-}
instance (FinalizationBaseMonad r s m) => FinalizationMonad (ActiveFinalizationM r s m) where
    finalizationBlockArrival = notifyBlockArrival
    finalizationBlockFinal fr b = (notifyBlockFinalized fr b)
    finalizationReceiveMessage = receiveFinalizationPseudoMessage
    finalizationReceiveRecord b fr = receiveFinalizationRecord b fr
    finalizationUnsettledRecordAt = getQueuedFinalization
    finalizationUnsettledRecords = getQueuedFinalizationsBeyond
