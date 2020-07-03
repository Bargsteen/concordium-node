{-# LANGUAGE
    RecordWildCards,
    TupleSections,
    FlexibleContexts,
    MonoLocalBinds,
    ScopedTypeVariables #-}
{-# OPTIONS_GHC -Wno-deprecations #-}
{-# LANGUAGE UndecidableInstances #-}
module GlobalStateTests.Accounts where

import Prelude hiding (fail)
import Control.Monad hiding (fail)
import Control.Monad.Fail
import Control.Monad.IO.Class
import Control.Monad.Reader (MonadReader)
import Control.Monad.Trans.Reader
import Control.Exception
import Data.Maybe
import qualified Data.Set as Set
import Data.Proxy
import Data.Serialize as S
import Data.Either
import Lens.Micro.Platform
import qualified Data.PQueue.Prio.Max as Queue
import qualified Data.Map.Strict as OrdMap
import System.IO.Temp
import System.FilePath

import qualified Data.FixedByteString as FBS
import Concordium.Types.HashableTo
import qualified Concordium.Crypto.SHA256 as H
import qualified Concordium.Crypto.SignatureScheme as Sig
import Concordium.Crypto.DummyData
import Concordium.ID.DummyData
import qualified Concordium.ID.Types as ID

import Concordium.GlobalState.Account
import Concordium.GlobalState.Persistent.BlobStore
import Concordium.GlobalState.Basic.BlockState.Account as BA
import qualified Concordium.GlobalState.Basic.BlockState.Accounts as B
import qualified Concordium.GlobalState.Basic.BlockState.AccountTable as BAT
import qualified Concordium.GlobalState.Persistent.Account as PA
import qualified Concordium.GlobalState.Persistent.Accounts as P
import qualified Concordium.GlobalState.Persistent.AccountTable as PAT
import qualified Concordium.GlobalState.Persistent.Trie as Trie
import Concordium.Types

import Test.Hspec
import Test.HUnit
import Test.QuickCheck

assertRight :: Either String a -> Assertion
assertRight (Left e) = assertFailure e
assertRight _ = return ()

checkBinary :: (Show a, MonadFail m) => (a -> a -> Bool) -> a -> a -> String -> String -> String -> m ()
checkBinary bop x y sbop sx sy = unless (bop x y) $ fail $ "Not satisfied: " ++ sx ++ " (" ++ show x ++ ") " ++ sbop ++ " " ++ sy ++ " (" ++ show y ++ ")"

checkBinaryM :: (Monad m, Show a, Show b, MonadFail m) => (a -> b -> m Bool) -> a -> b -> String -> String -> String -> m ()
checkBinaryM bop x y sbop sx sy = do
  satisfied <- bop x y
  unless satisfied $ fail $ "Not satisfied: " ++ sx ++ " (" ++ show x ++ ") " ++ sbop ++ " " ++ show y ++ " (" ++ sy ++ ")"

-- |Check that a 'B.Accounts' and a 'P.Accounts' are equivalent.
-- That is, they have the same account map, account table, and set of
-- use registration ids.
checkEquivalent :: (MonadReader r m, HasBlobStore r, MonadFail m, MonadIO m) => B.Accounts -> P.Accounts -> m ()
checkEquivalent ba pa = do
    pam <- Trie.toMap (P.accountMap pa)
    checkBinary (==) (B.accountMap ba) pam "==" "Basic account map" "Persistent account map"
    let bat = BAT.toList (B.accountTable ba)
    pat <- PAT.toList (P.accountTable pa)
    checkBinaryM sameAccList bat pat "==" "Basic account table (as list)" "Persistent account table (as list)"
    let bath = getHash (B.accountTable ba) :: H.Hash
    let path = getHash (P.accountTable pa) :: H.Hash
    checkBinary (==) bath path "==" "Basic account table hash" "Persistent account table hash"
    (pregids, _) <- P.loadRegIds pa
    checkBinary (==) (B.accountRegIds ba) pregids "==" "Basic registration ids" "Persistent registration ids"

    where -- Check whether an in-memory account-index and account pair is equivalent to a persistent account-index and account pair
          sameAccPair :: (MonadIO m, MonadBlobStore m BlobRef)
                      => Bool -- accumulator for the fold in 'sameAccList'
                      -> ((BAT.AccountIndex, BA.Account), (PAT.AccountIndex, PA.PersistentAccount)) -- the pairs to be compared
                      -> m Bool
          sameAccPair b ((bInd, bAcc), (pInd, pAcc)) = do
            sameAcc <- PA.sameAccount bAcc pAcc
            return $ b && bInd == pInd && sameAcc
          -- Check whether a list of in-memory account-index and account pairs is equivalent to a persistent list of account-index and account pairs
          sameAccList l1 l2 = foldM sameAccPair True $ zip l1 l2

data AccountAction
    = PutAccount Account
    | Exists AccountAddress
    | GetAccount AccountAddress
    | UpdateAccount AccountAddress (Account -> Account)
    | UnsafeGetAccount AccountAddress
    | RegIdExists ID.CredentialRegistrationID
    | RecordRegId ID.CredentialRegistrationID
    | FlushPersistent
    | ArchivePersistent

randomizeAccount :: AccountAddress -> ID.AccountKeys -> Gen Account
randomizeAccount _accountAddress _accountVerificationKeys = do
        _accountNonce <- Nonce <$> arbitrary
        _accountAmount <- Amount <$> arbitrary
        let _accountEncryptedAmount = []
        let _accountEncryptionKey = ID.makeEncryptionKey (dummyRegId _accountAddress)
        let _accountCredentials = Queue.empty
        let _accountStakeDelegate = Nothing
        let _accountInstances = mempty
        return Account{..}

randomCredential :: Gen ID.CredentialRegistrationID
randomCredential = ID.RegIdCred . FBS.pack <$> vectorOf 42 arbitrary

randomActions :: Gen [AccountAction]
randomActions = sized (ra Set.empty Set.empty)
    where
        randAccount = do
            address <- ID.AccountAddress . FBS.pack <$> vector ID.accountAddressSize
            n <- choose (1,255)
            akKeys <- OrdMap.fromList . zip [0..] . map Sig.correspondingVerifyKey <$> replicateM n genSigSchemeKeyPair
            akThreshold <- fromIntegral <$> choose (1,n)
            return (ID.AccountKeys{..}, address)
        ra _ _ 0 = return []
        ra s rids n = oneof $ [
                putRandAcc,
                exRandAcc,
                getRandAcc,
                (FlushPersistent:) <$> ra s rids (n-1),
                (ArchivePersistent:) <$> ra s rids (n-1),
                exRandReg,
                recRandReg,
                updateRandAcc
                ] ++ if null s then [] else [putExAcc, exExAcc, getExAcc, unsafeGetExAcc, updateExAcc]
                ++ if null rids then [] else [exExReg, recExReg]
            where
                putRandAcc = do
                    (vk, addr) <- randAccount
                    acct <- randomizeAccount addr vk
                    (PutAccount acct:) <$> ra (Set.insert (vk, addr) s) rids (n-1)
                putExAcc = do
                    (vk, addr) <- elements (Set.toList s)
                    acct <- randomizeAccount addr vk
                    (PutAccount acct:) <$> ra s rids (n-1)
                exRandAcc = do
                    (_, addr) <- randAccount
                    (Exists addr:) <$> ra s rids (n-1)
                exExAcc = do
                    (_, addr) <- elements (Set.toList s)
                    (Exists addr:) <$> ra s rids (n-1)
                getRandAcc = do
                    (_, addr) <- randAccount
                    (GetAccount addr:) <$> ra s rids (n-1)
                getExAcc = do
                    (_, addr) <- elements (Set.toList s)
                    (GetAccount addr:) <$> ra s rids (n-1)
                updateExAcc = do
                    (_, addr) <- elements (Set.toList s)
                    newNonce <- Nonce <$> arbitrary
                    newAmount <- Amount <$> arbitrary
                    let upd acc = if BA._accountAddress acc == addr
                            then
                                acc {_accountAmount = newAmount, _accountNonce = newNonce}
                            else
                                error "address does not match expected value"
                    (UpdateAccount addr upd:) <$> ra s rids (n-1)
                updateRandAcc = do
                    (vk, addr) <- randAccount
                    let upd _ = error "account address should not exist"
                    if (vk, addr) `Set.member` s then
                        ra s rids n
                    else
                        (UpdateAccount addr upd:) <$> ra s rids (n-1)
                unsafeGetExAcc = do
                    (_, addr) <- elements (Set.toList s)
                    (UnsafeGetAccount addr:) <$> ra s rids (n-1)
                exRandReg = do
                    rid <- randomCredential
                    (RegIdExists rid:) <$> ra s rids (n-1)
                exExReg = do
                    rid <- elements (Set.toList rids)
                    (RegIdExists rid:) <$> ra s rids (n-1)
                recRandReg = do
                    rid <- randomCredential
                    (RecordRegId rid:) <$> ra s (Set.insert rid rids) (n-1)
                recExReg = do -- This is not an expected case in practice
                    rid <- elements (Set.toList rids)
                    (RecordRegId rid:) <$> ra s rids (n-1)


makePureAccount :: (MonadIO m, MonadBlobStore m BlobRef) => PA.PersistentAccount -> m Account
makePureAccount PA.PersistentAccount{..} = do
  PersistingAccountData{..} <- loadBufferedRef _persistingData
  return Account{..}

runAccountAction :: (MonadBlobStore m BlobRef, MonadReader r m, HasBlobStore r, MonadFail m, MonadIO m) => AccountAction -> (B.Accounts, P.Accounts) -> m (B.Accounts, P.Accounts)
runAccountAction (PutAccount acct) (ba, pa) = do
        let ba' = B.putAccount acct ba
        pAcct <- PA.makePersistentAccount acct
        pa' <- P.putAccount pAcct pa
        return (ba', pa')
runAccountAction (Exists addr) (ba, pa) = do
        let be = B.exists addr ba
        pe <- P.exists addr pa
        checkBinary (==) be pe "<->" "account exists in basic" "account exists in persistent"
        return (ba, pa)
runAccountAction (GetAccount addr) (ba, pa) = do
        let bacct = B.getAccount addr ba
        pacct <- P.getAccount addr pa
        let sameAcc (Just ba) (Just pa) = PA.sameAccount ba pa
            sameAcc Nothing Nothing = return True
            sameAcc _ _ = return False
        checkBinaryM sameAcc bacct pacct "==" "account in basic" "account in persistent"
        return (ba, pa)
runAccountAction (UpdateAccount addr upd) (ba, pa) = do
        let ba' = ba & ix addr %~ upd
            -- Transform a function that updates in-memory accounts into a function that updates persistent accounts
            liftP :: (MonadIO m, MonadBlobStore m BlobRef) => (Account -> Account) -> PA.PersistentAccount -> m PA.PersistentAccount
            liftP f pAcc = do
              bAcc <- makePureAccount pAcc
              PA.makePersistentAccount $ f bAcc
        (_, pa') <- P.updateAccounts (fmap ((),) . liftP upd) addr pa
        return (ba', pa')
runAccountAction (UnsafeGetAccount addr) (ba, pa) = do
        let bacct = B.unsafeGetAccount addr ba
        pacct <- P.unsafeGetAccount addr pa
        checkBinaryM PA.sameAccount bacct pacct "==" "account in basic" "account in persistent"
        return (ba, pa)
runAccountAction FlushPersistent (ba, pa) = do
        (_, pa') <- storeUpdate (Proxy :: Proxy BlobRef) pa
        return (ba, pa')
runAccountAction ArchivePersistent (ba, pa) = do
        ppa <- store (Proxy :: Proxy BlobRef) pa
        pa' <- fromRight (error "deserializing blob failed") $ S.runGet (load (Proxy :: Proxy BlobRef)) (S.runPut ppa)
        return (ba, pa')
runAccountAction (RegIdExists rid) (ba, pa) = do
        let be = B.regIdExists rid ba
        (pe, pa') <- P.regIdExists rid pa
        checkBinary (==) be pe "<->" "regid exists in basic" "regid exists in persistent"
        return (ba, pa')
runAccountAction (RecordRegId rid) (ba, pa) = do
        let ba' = B.recordRegId rid ba
        pa' <- P.recordRegId rid pa
        return (ba', pa')

emptyTest :: SpecWith BlobStore
emptyTest = it "empty" $ runReaderT
            (checkEquivalent B.emptyAccounts P.emptyAccounts :: ReaderT BlobStore IO ())

actionTest :: Word -> SpecWith BlobStore
actionTest lvl = it "account actions" $ \bs -> withMaxSuccess (100 * fromIntegral lvl) $ property $ do
        acts <- randomActions
        return $ ioProperty $ flip runReaderT bs $ do
            (ba, pa) <- foldM (flip runAccountAction) (B.emptyAccounts, P.emptyAccounts) acts
            checkEquivalent ba pa


tests :: Word -> Spec
tests lvl = describe "GlobalStateTests.Accounts" $
            around (\kont ->
                      withTempDirectory "." "blockstate" $ \dir ->
                       createBlobStore (dir </> "blockstate.dat") >>= kont
                   ) $ do emptyTest
                          actionTest lvl
