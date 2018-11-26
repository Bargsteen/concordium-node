{-# LANGUAGE ForeignFunctionInterface #-}
module Concordium.External where

import Foreign
import Foreign.C
import Control.Concurrent.Chan
import Control.Concurrent
import System.Random
import qualified Data.ByteString.Char8 as BS
import qualified Data.Map as Map
import Data.Serialize

import qualified Concordium.Crypto.DummySignature as Sig
import qualified Concordium.Crypto.DummyVRF as VRF

import Concordium.Types
import Concordium.Birk.Bake
import Concordium.Payload.Transaction
import Concordium.Runner
import Concordium.Show

import qualified Concordium.Startup as S

-- Test functions

-- triple :: Int64 -> IO ()
-- triple x = print x

-- foreign export ccall triple :: Int64 -> IO ()

-- foreign import ccall "dynamic" mkCallback :: FunPtr (I32 -> IO I32) -> I32 -> IO I32

-- callbackTwice :: FunPtr (I32 -> IO I32) -> IO I32
-- callbackTwice fun = do
--     let f = mkCallback fun
--     n <- f 0
--     f n


-- foreign export ccall callbackTwice :: FunPtr (I32 -> IO I32) -> IO I32

-- printCString :: CString -> IO ()
-- printCString cs = do
--     s <- peekCString cs
--     putStrLn s

-- foreign export ccall printCString :: CString -> IO ()

-- foreign import ccall "dynamic" mkCStringCallback :: FunPtr (CString -> IO ()) -> CString -> IO ()

-- callbackWithCString :: FunPtr (CString -> IO ()) -> IO ()
-- callbackWithCString cb =
--     withCString "Hello cruel world!" (mkCStringCallback cb)

-- foreign export ccall callbackWithCString :: FunPtr (CString -> IO ()) -> IO ()

data BakerRunner = BakerRunner {
    bakerInChan :: Chan InMessage,
    bakerOutChan :: Chan OutMessage
}

type CStringCallback = CString -> Int64 -> IO ()
foreign import ccall "dynamic" callCStringCallback :: FunPtr CStringCallback -> CStringCallback

foreign import ccall "dynamic" callCStringCallbackInstance :: FunPtr (Int64 -> CStringCallback) -> Int64 -> CStringCallback

makeGenesisData :: 
    Timestamp -- ^Genesis time
    -> Word64 -- ^Number of bakers
    -> FunPtr CStringCallback -- ^Function to process the generated genesis data.
    -> FunPtr (Int64 -> CStringCallback) -- ^Function to process each baker identity. Will be called repeatedly with different baker ids.
    -> IO ()
makeGenesisData genTime nBakers cbkgen cbkbaker = do
    BS.useAsCStringLen (encode genData) $ \(cdata, clen) -> callCStringCallback cbkgen cdata (fromIntegral clen)
    mapM_ (\bkr@(BakerIdentity bid _ _) -> BS.useAsCStringLen (encode bkr) $ \(cdata, clen) -> callCStringCallbackInstance cbkbaker (fromIntegral bid) cdata (fromIntegral clen)) bakersPrivate
    where
        bakers = S.makeBakers (fromIntegral nBakers)
        genData = S.makeGenesisData genTime bakers
        bakersPrivate = map fst bakers

type BlockCallback = CString -> Int64 -> IO ()

foreign import ccall "dynamic" callBlockCallback :: FunPtr BlockCallback -> BlockCallback

outLoop :: Chan OutMessage -> BlockCallback -> IO ()
outLoop chan cbk = do
    (MsgNewBlock block) <- readChan chan
    let bbs = runPut (serializeBlock block)
    BS.useAsCStringLen bbs $ \(cstr, l) -> cbk cstr (fromIntegral l)
    outLoop chan cbk

startBaker :: 
           CString -> Int64 -- ^Serialized genesis data (c string + len)
           -> CString -> Int64 -- ^Serialized baker identity (c string + len)
           -> FunPtr BlockCallback -> IO (StablePtr BakerRunner)
startBaker gdataC gdataLenC bidC bidLenC bcbk = do
    gdata <- BS.packCStringLen (gdataC, fromIntegral gdataLenC)
    bdata <- BS.packCStringLen (bidC, fromIntegral bidLenC)
    case (decode gdata, decode bdata) of
      (Right genData, Right bid) -> do
        (cin, cout) <- makeRunner bid genData
        forkIO $ outLoop cout (callBlockCallback bcbk)
        newStablePtr (BakerRunner cin cout)
      _   -> ioError (userError $ "Error decoding serialized data.")

stopBaker :: StablePtr BakerRunner -> IO ()
stopBaker bptr = do
    BakerRunner cin _ <- deRefStablePtr bptr
    freeStablePtr bptr
    writeChan cin MsgShutdown

receiveBlock :: StablePtr BakerRunner -> CString -> Int64 -> IO ()
receiveBlock bptr cstr l = do
    BakerRunner cin _ <- deRefStablePtr bptr
    blockBS <- BS.packCStringLen (cstr, fromIntegral l)
    case runGet deserializeBlock blockBS of
        Left _ -> return ()
        Right block -> writeChan cin $ MsgBlockReceived block

printBlock :: CString -> Int64 -> IO ()
printBlock cstr l = do
    blockBS <- BS.packCStringLen (cstr, fromIntegral l)
    case runGet deserializeBlock blockBS of
        Left _ -> putStrLn "<Bad Block>"
        Right block -> putStrLn $ showsBlock block ""

receiveTransaction :: StablePtr BakerRunner -> Word64 -> Word64 -> Word64 -> Word64 -> CString -> Int64 -> IO ()
receiveTransaction bptr n0 n1 n2 n3 tdata tlen = do
    BakerRunner cin _ <- deRefStablePtr bptr
    tbs <- BS.packCStringLen (tdata, fromIntegral tlen)
    writeChan cin $ MsgTransactionReceived (Transaction (TransactionNonce n0 n1 n2 n3) tbs)

foreign export ccall makeGenesisData :: Timestamp -> Word64 -> FunPtr CStringCallback -> FunPtr (Int64 -> CStringCallback) -> IO ()
foreign export ccall startBaker :: CString -> Int64 -> CString -> Int64 -> FunPtr BlockCallback -> IO (StablePtr BakerRunner)
foreign export ccall stopBaker :: StablePtr BakerRunner -> IO ()
foreign export ccall receiveBlock :: StablePtr BakerRunner -> CString -> Int64 -> IO ()
foreign export ccall printBlock :: CString -> Int64 -> IO ()
foreign export ccall receiveTransaction :: StablePtr BakerRunner -> Word64 -> Word64 -> Word64 -> Word64 -> CString -> Int64 -> IO ()
