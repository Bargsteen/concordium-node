
{-# LANGUAGE RecordWildCards, MultiParamTypeClasses, FunctionalDependencies, TypeFamilies, FlexibleInstances, QuantifiedConstraints,
    GeneralizedNewtypeDeriving, BangPatterns, StandaloneDeriving, UndecidableInstances, DefaultSignatures, DeriveFunctor, ConstraintKinds, RankNTypes,
    ScopedTypeVariables, TupleSections, DeriveFoldable, DeriveTraversable, DerivingStrategies, FlexibleContexts, DeriveGeneric #-}
{-|

-}
module Concordium.GlobalState.Persistent.BlobStore where

import Control.Concurrent.MVar
import System.IO
import Data.Serialize
import Data.Word
import qualified Data.ByteString as BS
import Control.Exception
import GHC.Generics
import Data.Functor.Foldable
import Control.Monad.Reader.Class
import Control.Monad.Trans.Reader (ReaderT, runReaderT)
import Control.Monad.IO.Class
import System.Directory
import Data.Proxy
import GHC.Stack
import Data.IORef
import Concordium.GlobalState.Bakers (Bakers)

import Concordium.GlobalState.Persistent.MonadicRecursive

-- Imports for providing instances
import qualified Concordium.GlobalState.IdentityProviders as IPS
import qualified Concordium.GlobalState.Parameters as Parameters
import Concordium.Types (Account)

newtype BlobRef a = BlobRef Word64
    deriving (Eq, Ord, Serialize)

instance Show (BlobRef a) where
    show (BlobRef v) = '@' : show v

nullRef :: BlobRef a
nullRef = BlobRef maxBound

data BlobStore = BlobStore {
    blobStoreFile :: !(MVar Handle),
    blobStoreFilePath :: !FilePath
}

class HasBlobStore a where
    blobStore :: a -> BlobStore

instance HasBlobStore BlobStore where
    blobStore = id

createTempBlobStore :: FilePath -> IO BlobStore
createTempBlobStore blobStoreFilePath = do
    h <- openBinaryFile blobStoreFilePath ReadWriteMode
    blobStoreFile <- newMVar h
    return $! BlobStore{..}

destroyTempBlobStore :: BlobStore -> IO ()
destroyTempBlobStore BlobStore{..} = do
    h <- takeMVar blobStoreFile
    hClose h

runBlobStoreTemp :: FilePath -> ReaderT BlobStore IO a -> IO a
runBlobStoreTemp dir a = bracket openf closef usef
    where
        openf = openBinaryTempFile dir "blb.dat"
        closef (tempFP, h) = do
            hClose h
            removeFile tempFP
        usef (fp, h) = do
            mv <- newMVar h
            res <- runReaderT a (BlobStore mv fp)
            _ <- takeMVar mv
            return res

readBlobBS :: BlobStore -> BlobRef a -> IO BS.ByteString
readBlobBS BlobStore{..} (BlobRef offset) = bracketOnError (takeMVar blobStoreFile) (tryPutMVar blobStoreFile) $ \h -> do
        hSeek h AbsoluteSeek (fromIntegral offset)
        esize <- decode <$> BS.hGet h 8
        case esize :: Either String Word64 of
            Left e -> error e
            Right size -> do
                bs <- BS.hGet h (fromIntegral size)
                putMVar blobStoreFile h
                return bs

readBlob :: (Serialize a) => BlobStore -> BlobRef a -> IO a
readBlob bstore ref = do
        bs <- readBlobBS bstore ref
        case decode bs of
            Left e -> error e
            Right v -> return v

writeBlobBS ::BlobStore -> BS.ByteString -> IO (BlobRef a)
writeBlobBS BlobStore{..} bs = bracketOnError (takeMVar blobStoreFile) (tryPutMVar blobStoreFile) $ \h -> do
        hSeek h SeekFromEnd 0
        offset <- fromIntegral <$> hTell h
        BS.hPut h size
        BS.hPut h bs
        putMVar blobStoreFile h
        return (BlobRef offset)
    where
        size = encode (fromIntegral (BS.length bs) :: Word64)

writeBlob :: (Serialize a) => BlobStore -> a -> IO (BlobRef a)
writeBlob bstore v = writeBlobBS bstore (encode v)

class (Monad m, forall a. Serialize (ref a)) => MonadBlobStore m ref where
    storeRaw :: BS.ByteString -> m (ref a)
    loadRaw :: ref a -> m BS.ByteString
    storeBlob :: (Serialize a) => a -> m (ref a)
    storeBlob = storeRaw . encode
    loadBlob :: (Serialize a) => ref a -> m a
    loadBlob r = do
        bs <- loadRaw r
        case decode bs of
            Left e -> error e
            Right v -> return v

instance (MonadIO m, MonadReader r m, HasBlobStore r) => MonadBlobStore m BlobRef where
    storeRaw b = do
        bs <- blobStore <$> ask
        liftIO $ writeBlobBS bs b
    loadRaw r = do
        bs <- blobStore <$> ask
        liftIO $ readBlobBS bs r

-- |The @BlobStorable m ref a@ class defines how a value
-- of type @a@ may be stored as in a reference of type @ref a@
-- in the monad @m@.
--
-- Where @a@ is an instance of 'Serialize', default implementations
-- are provided for 'store' and 'load' that simply (de)serialize
-- the value.  For a complex datatype that uses internal pointers,
-- 'store' and 'load' are expected to translate between such pointers
-- and references in the underlying store.
class (MonadBlobStore m ref) => BlobStorable m ref a where
    -- |Serialize a value of type @a@ for storage.
    store :: Proxy ref -> a -> m Put
    default store :: (Serialize a) => Proxy ref -> a -> m Put
    store _ = pure . put
    -- |Deserialize a value of type @a@ from storage.
    load :: Proxy ref -> Get (m a)
    default load :: (Serialize a) => Proxy ref -> Get (m a)
    load _ = pure <$> get
    -- |Store a value of type @a@, possibly updating its representation.
    -- This is used when the value's representation includes pointers that
    -- may be replaced or supplemented with blob references.
    storeUpdate :: Proxy ref -> a -> m (Put, a)
    storeUpdate p v = (,v) <$> store p v

    storeRef :: a -> m (ref a)
    storeRef v = do
        p <- runPut <$> store (Proxy :: Proxy ref) v
        storeRaw p
    storeUpdateRef :: a -> m (ref a, a)
    storeUpdateRef v = do
        (p, v') <- storeUpdate (Proxy :: Proxy ref) v
        (, v') <$> storeRaw (runPut p)
    loadRef :: (HasCallStack) => (ref a) -> m a
    loadRef ref = do
        bs <- loadRaw ref
        case runGet (load (Proxy :: Proxy ref)) bs of
            Left e -> error (e ++ " :: " ++ show bs)
            Right !mv -> mv

newtype SerializeStorable v = SerStore v
    deriving newtype (Eq, Ord, Show, Serialize)

instance (Serialize v, MonadBlobStore m ref) => BlobStorable m ref (SerializeStorable v)

data Nullable v = Null | Some !v
    deriving (Eq, Ord, Show, Functor, Foldable, Traversable)

instance Serialize (Nullable (BlobRef a)) where
    put Null = put nullRef
    put (Some v) = put v
    get = do
        r <- get
        return $! if r == nullRef then Null else Some r

instance (MonadBlobStore m BlobRef) => BlobStorable m BlobRef (BlobRef a)
instance (MonadBlobStore m BlobRef) => BlobStorable m BlobRef (Nullable (BlobRef a))

data CachedRef a
    = CRBlobbed {crRef :: !(BlobRef a)}
    | CRCached {crRef :: !(BlobRef a), crValue :: !a}

instance (MonadBlobStore m BlobRef) => BlobStorable m BlobRef (CachedRef a) where
    store p v = store p (crRef v)
    load p = fmap CRBlobbed <$> load p

instance (MonadBlobStore m BlobRef) => BlobStorable m BlobRef (Nullable (CachedRef a)) where
    store p v = store p (crRef <$> v)
    load p = fmap (fmap CRBlobbed) <$> load p

-- |A value that may exists purely on disk ('BRBlobbed'), purely in memory ('BRMemory'), or both ('BRCached').
-- When the value is cached, the cached value must match the value stored on disk.
data BufferedRef a
    = BRBlobbed {brRef :: !(BlobRef a)}
    -- ^Value stored on disk
    | BRMemory {brIORef :: !(IORef (BlobRef a)), brValue :: !a}
    -- ^Value stored in memory and possibly on disk.
    -- When a new 'BRMemory' instance is created, we initialize 'brIORef' to 'nullRef'.
    -- When we store the instance in persistent storage, we update 'brIORef' with the corresponding pointer.
    -- That way, when we store the same instance again on disk (this could be, e.g., a child block
    -- that inherited its parent's state) we can store the pointer to the 'brValue' data rather than
    -- storing all of the data again.

makeBRMemory :: (MonadIO m) => (BlobRef a) -> a -> m (BufferedRef a)
makeBRMemory r a = liftIO $ do
    ref <- newIORef r
    return $ BRMemory ref a

makeBufferedRef :: (MonadIO m) => a -> m (BufferedRef a)
makeBufferedRef = makeBRMemory nullRef

instance Show a => Show (BufferedRef a) where
    show (BRBlobbed r) = show r
    show (BRMemory _ v) = "{" ++ show v ++ "}"

instance (BlobStorable m BlobRef a, MonadIO m) => BlobStorable m BlobRef (BufferedRef a) where
    store p b = getBRRef b >>= store p
    load p = fmap BRBlobbed <$> load p
    storeUpdate p brm@(BRMemory ref v) = do
        r <- liftIO $ readIORef ref
        if r == nullRef
        then do
            (r' :: BlobRef a, v') <- storeUpdateRef v
            liftIO $ writeIORef ref r'
            (,BRMemory ref v') <$> store p r'
        else (,brm) <$> store p brm
    storeUpdate p x = (,x) <$> store p x

-- |Stores in-memory data to disk if it has not been stored yet and returns pointer to saved data
getBRRef :: (BlobStorable m BlobRef a, MonadIO m) => BufferedRef a -> m (BlobRef a)
getBRRef (BRMemory ref v) = do
    r <- liftIO $ readIORef ref
    if r == nullRef
    then do
        (r' :: BlobRef a) <- storeRef v
        liftIO $ writeIORef ref r'
        return r'
    else
        return r
getBRRef (BRBlobbed r) = return r

instance (BlobStorable m BlobRef a, MonadIO m) => BlobStorable m BlobRef (Nullable (BufferedRef a)) where
    store _ Null = return $ put nullRef
    store p (Some v) = store p v
    load p = do
        r <- get
        if r == nullRef then
            return (pure Null)
        else
            fmap Some <$> load p
    storeUpdate _ n@Null = return (put nullRef, n)
    storeUpdate p (Some v) = do
        (r, v') <- storeUpdate p v
        return (r, Some v')

loadBufferedRef :: (HasCallStack, BlobStorable m BlobRef a) => BufferedRef a -> m a
loadBufferedRef (BRBlobbed ref) = loadRef ref
loadBufferedRef (BRMemory _ v) = return v

-- |Load a 'BufferedRef' and cache it if it wasn't already in memory
cacheBufferedRef :: (HasCallStack, BlobStorable m BlobRef a, MonadIO m) => BufferedRef a -> m (a, BufferedRef a)
cacheBufferedRef (BRBlobbed ref) = do
        v <- loadRef ref
        (v,) <$> makeBRMemory ref v
cacheBufferedRef r@(BRMemory _ v) = return (v, r)


{-
class RefStorable ref m x where
    makeRef :: (forall a. ref a -> m Put) -> x -> m (ref x)
    loadRef :: (forall a. Get (m a)) -> m (ref x) -> x
-}

cachedToBlob :: CachedRef a -> BlobRef a
cachedToBlob = crRef

blobToCached :: BlobRef a -> CachedRef a
blobToCached = CRBlobbed

flushBufferedRef :: (BlobStorable m BlobRef a, MonadIO m) => BufferedRef a -> m (BufferedRef a, BlobRef a)
flushBufferedRef brm@(BRMemory ref v) = do
    r <- liftIO $ readIORef ref
    if r == nullRef
    then do
        (r' :: BlobRef a, v') <- storeUpdateRef v
        liftIO $ writeIORef ref r'
        (,r') <$> makeBRMemory r' v'
    else
        return (brm, r)
flushBufferedRef b = return (b, brRef b)

uncacheBuffered :: (BlobStorable m BlobRef a, MonadIO m) => BufferedRef a -> m (BufferedRef a)
uncacheBuffered v@(BRMemory _ _) = BRBlobbed <$> getBRRef v
uncacheBuffered b = return b


newtype Blobbed ref f = Blobbed {unblobbed :: ref (f (Blobbed ref f)) }

deriving instance (forall a. Serialize (ref a)) => Serialize (Blobbed ref f)

instance (MonadBlobStore m ref) => BlobStorable m ref (Blobbed ref f)

instance (forall a. Serialize (Nullable (ref a))) => Serialize (Nullable (Blobbed ref f)) where
    put = put . fmap unblobbed
    get = fmap Blobbed <$> get

instance (MonadBlobStore m ref, forall a. Serialize (Nullable (ref a))) => BlobStorable m ref (Nullable (Blobbed ref f)) where

type instance Base (Blobbed ref f) = f

instance (MonadBlobStore m ref, BlobStorable m ref (f (Blobbed ref f))) => MRecursive m (Blobbed ref f) where
    mproject (Blobbed r) = loadRef r

instance (MonadBlobStore m ref, BlobStorable m ref (f (Blobbed ref f))) => MCorecursive m (Blobbed ref f) where
    membed r = Blobbed <$> storeRef r

class HasNull ref where
    refNull :: ref
    isNull :: ref -> Bool

instance HasNull (Blobbed BlobRef a) where
    refNull = Blobbed nullRef
    isNull (Blobbed r) = r == nullRef

data CachedBlobbed ref f
    = CBUncached (Blobbed ref f)
    | CBCached (Blobbed ref f) (f (CachedBlobbed ref f))

cachedBlob :: CachedBlobbed ref f -> Blobbed ref f
cachedBlob (CBUncached r) = r
cachedBlob (CBCached r _) = r

type instance Base (CachedBlobbed ref f) = f

instance (MonadBlobStore m ref, BlobStorable m ref (f (Blobbed ref f)), Functor f) => MRecursive m (CachedBlobbed ref f) where
    mproject (CBUncached r) = fmap CBUncached <$> mproject r
    mproject (CBCached _ c) = pure c

instance (MonadBlobStore m ref, BlobStorable m ref (f (Blobbed ref f)), Functor f) => MCorecursive m (CachedBlobbed ref f) where
    membed r = do
        b <- membed (fmap cachedBlob r)
        return (CBCached b r)

instance (forall a. Serialize (ref a)) => Serialize (CachedBlobbed ref f) where
    put = put . cachedBlob
    get = CBUncached <$> get

instance (MonadBlobStore m ref) => BlobStorable m ref (CachedBlobbed ref f)



-- TODO (MRA) rename
data BufferedBlobbed ref f
    = LBMemory (IORef (Blobbed ref f)) (f (BufferedBlobbed ref f))
    | LBCached (CachedBlobbed ref f)

makeLBMemory :: (MonadIO m) => Blobbed ref f -> f (BufferedBlobbed ref f) -> m (BufferedBlobbed ref f)
makeLBMemory r a = liftIO $ do
    ref <- newIORef r
    return $ LBMemory ref a

makeBufferedBlobbed :: (MonadIO m, HasNull (Blobbed ref f)) => f (BufferedBlobbed ref f) -> m (BufferedBlobbed ref f)
makeBufferedBlobbed = makeLBMemory refNull

type instance Base (BufferedBlobbed ref f) = f

instance (MonadBlobStore m ref, BlobStorable m ref (f (Blobbed ref f)), Functor f) => MRecursive m (BufferedBlobbed ref f) where
    mproject (LBMemory _ r) = pure r
    mproject (LBCached c) = fmap LBCached <$> mproject c
    {-# INLINE mproject #-}

instance (MonadIO m, HasNull (Blobbed ref f)) => MCorecursive m (BufferedBlobbed ref f) where
    membed = makeBufferedBlobbed
    {-# INLINE membed #-}

-- |Stores in-memory data to disk if it has not been stored yet and returns pointer to saved data
getBBRef :: (BlobStorable m ref (BufferedBlobbed ref f), BlobStorable m ref (f (Blobbed ref f)), MonadIO m, HasNull (Blobbed ref f), Traversable f)
               => Proxy ref
               -> BufferedBlobbed ref f
               -> m ((Put, BufferedBlobbed ref f), Blobbed ref f)
getBBRef p v@(LBCached c) = (, cachedBlob c) . (, v) <$> store p c
getBBRef p v@(LBMemory ref _) = do
    r <- liftIO $ readIORef ref
    if isNull r
    then do
        (pu, cb) <- storeAndGetCached v
        return ((pu, LBCached cb), cachedBlob cb)
    else
        getBBRef p (LBCached (CBUncached r))
    where storeAndGetCached (LBCached c) = storeUpdate p c
          storeAndGetCached (LBMemory ref' t) = do
            t' <- mapM (fmap snd . storeAndGetCached) t
            rm <- liftIO $ readIORef ref'
            if (isNull rm)
            then do
                r <- storeRef (cachedBlob <$> t')
                liftIO $ writeIORef ref' (Blobbed r)
                return (put r, CBCached (Blobbed r) t')
            else storeUpdate p (CBCached rm t')

instance (MonadIO m, MonadBlobStore m ref, Traversable f, BlobStorable m ref (f (Blobbed ref f)), HasNull (Blobbed ref f))
         => BlobStorable m ref (BufferedBlobbed ref f) where
    store p v = getBBRef p v >>= store p . snd

    storeUpdate p v = fst <$> getBBRef p v

    load _ = return . LBCached <$> get

class FixShowable fix where
    showFix :: Functor f => (f String -> String) -> fix f -> String

instance (forall a. Show (ref a)) => FixShowable (Blobbed ref) where
    showFix _ (Blobbed r) = show r

instance (forall a. Show (ref a)) => FixShowable (CachedBlobbed ref) where
    showFix sh (CBCached r v) = "{" ++ (sh (showFix sh <$> v)) ++ "}" ++ showFix sh r
    showFix sh (CBUncached r) = showFix sh r

instance (forall a. Show (ref a)) => FixShowable (BufferedBlobbed ref) where
    showFix sh (LBMemory _ v) = "{" ++ (sh (showFix sh <$> v)) ++ "}"
    showFix sh (LBCached r) = showFix sh r

-- BlobStorable instances
instance (MonadBlobStore m ref) => BlobStorable m ref IPS.IdentityProviders
instance (MonadBlobStore m ref) => BlobStorable m ref Parameters.CryptographicParameters
instance (MonadBlobStore m ref) => BlobStorable m ref Bakers
-- FIXME: This uses serialization of accounts for storing them.
-- This is potentially quite wasteful when only small changes are made.
instance (MonadBlobStore m ref) => BlobStorable m ref Account
instance (MonadBlobStore m ref) => BlobStorable m ref Word64
