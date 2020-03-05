{-# OPTIONS_GHC -Wall #-}
module Concordium.Scheduler.Types (module Concordium.Scheduler.Types,
                                   module Concordium.Types,
                                   module Concordium.Types.Execution,
                                   module Concordium.Types.Acorn.Interfaces,
                                   module Concordium.Types.Transactions,
                                   module Concordium.GlobalState.Instance,
                                   module Concordium.GlobalState.Rewards,
                                   module Concordium.GlobalState.Parameters,
                                   module Concordium.GlobalState.IdentityProviders,
                                   IdentityProviderIdentity,
                                   ReceiveContext(..),
                                   InitContext(..),
                                   linkWithMaxSize,
                                   Core.NoAnnot) where

import Prelude hiding(fail)

import Concordium.Types
import Concordium.Types.Acorn.Interfaces hiding(Value, Interface, ValueInterface, ContractValue)
import qualified Concordium.Types.Acorn.Interfaces as Interfaces
import Concordium.Types.Execution
import Concordium.GlobalState.Instance
import Concordium.GlobalState.Rewards
import Concordium.Types.Transactions
import Concordium.GlobalState.Parameters
import Concordium.GlobalState.IdentityProviders

import Concordium.ID.Types(IdentityProviderIdentity)

import Acorn.Types(ReceiveContext(..), InitContext(..), linkWithMaxSize)
import qualified Acorn.Core as Core

type Value = Interfaces.Value Core.NoAnnot
type ContractValue = Interfaces.LinkedContractValue Core.NoAnnot
type ValueInterface = Interfaces.UnlinkedValueInterface Core.NoAnnot
type Interface = Interfaces.Interface Core.UA

type Module = Core.Module Core.UA

dummyChainMeta :: ChainMetadata
dummyChainMeta = ChainMetadata { slotNumber = 0
                               , blockHeight = 0
                               , finalizedHeight = 0
                               , slotTime = 0
                               }


-- |Result type when constructing a block.
data FilteredTransactions msg = FilteredTransactions {
  -- |Transactions which have been added to the block, with results.
  ftAdded :: [(msg, TransactionSummary)],
  -- |Transactions which failed. No order is guaranteed.
  ftFailed :: [(msg, FailureKind)],
  -- |Transactions which were not processed since we reached block size limit.
  -- No order is guaranteed.
  ftUnprocessed :: [msg]
  }

-- |Transactions grouped by accounts.
-- For example, if T1 and T2 are transactions from account A,
-- and T3, T4, and T5 are transactions from account B, then
-- we group the transactions as [[T1, T2], [T3, T4, T5]].
type GroupedTransactions msg = [[msg]]
