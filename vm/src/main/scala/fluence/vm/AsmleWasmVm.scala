/*
 * Copyright 2018 Fluence Labs Limited
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package fluence.vm
import java.lang.reflect.Method

import asmble.compile.jvm.AsmExtKt
import cats.data.EitherT
import cats.effect.{IO, LiftIO}
import cats.{Functor, Monad, Traverse}
import fluence.crypto.Crypto.Hasher
import fluence.vm.VmError.WasmVmError.{GetVmStateError, InvokeError}
import fluence.vm.VmError.{InvalidArgError, _}
import fluence.vm.AsmleWasmVm._
import scodec.bits.ByteVector

import scala.language.higherKinds
import scala.util.Try

/**
 * Base implementation of [[WasmVm]].
 *
 * '''Note!!! This implementation isn't thread-safe. The provision of calls
 * linearization is the task of the caller side.'''
 *
 * @param functionsIndex the index for fast function searching. Contains all the
 *                     information needed to execute any function.
 * @param modules list of Wasm modules
 * @param hasher a hash function provider
 * @param allocateFunctionName name of function that will be used for allocation
 *                             memory in the Wasm part
 * @param deallocateFunctionName name of a function that will be used for freeing memory
 *                               that was previously allocated by allocateFunction
 */
class AsmleWasmVm(
  functionsIndex: WasmFnIndex,
  modules: WasmModules,
  hasher: Hasher[Array[Byte], Array[Byte]],
  allocateFunctionName: String,
  deallocateFunctionName: String
) extends WasmVm {

  // TODO: now it is assumed that allocation/deallocation functions placed together in the first module.
  // In the future it has to be refactored.
  // TODO: add handling of empty modules list.
  val allocateFunction: Option[WasmFunction] =
    functionsIndex.get(FunctionId(modules.head.name, AsmExtKt.getJavaIdent(allocateFunctionName)))

  val deallocateFunction: Option[WasmFunction] =
    functionsIndex.get(FunctionId(modules.head.name, AsmExtKt.getJavaIdent(deallocateFunctionName)))

  override def invoke[F[_]: LiftIO: Monad](
    moduleName: Option[String],
    fnName: String,
    fnArgs: Seq[String]
  ): EitherT[F, InvokeError, Option[Any]] = {
    val functionId = FunctionId(moduleName, AsmExtKt.getJavaIdent(fnName))

    for {
      // Finds java method(Wasm function) in the index by function id
      wasmFn <- EitherT
        .fromOption(
          functionsIndex.get(functionId),
          NoSuchFnError(s"Unable to find a function with the name=$functionId")
        )

      preprocessedArguments <- preprocessArguments(fnArgs, wasmFn.module)
      arguments ← parseArguments(preprocessedArguments, wasmFn)

      // invoke the function
      result ← wasmFn[F](arguments)
      // TODO : In the current version it is expected that callee (Wasm module) clean memory by itself
      // after construction of the result string. F.e. in Rust String object along with &str are the common
      // classes for operations on strings, in C++ the same role has std::string and std::stringview classes.
      // So, now it is expected that Rust/C++ methods at first construct objects of corresponding classes by
      // using injected string bytes as source and then delete these string bytes. But in the future it has
      // to be done by caller through deallocate function call.
    } yield if (wasmFn.javaMethod.getReturnType == Void.TYPE) None else Option(result)

  }

  override def getVmState[F[_]: LiftIO: Monad]: EitherT[F, GetVmStateError, ByteVector] =
    modules
      .foldLeft(EitherT.rightT[F, GetVmStateError](Array[Byte]())) {
        case (acc, instance) ⇒
          for {
            moduleStateHash ← instance
              .innerState(arr ⇒ hasher[F](arr))

            prevModulesHash ← acc

            concatHashes ← safeConcat(moduleStateHash, prevModulesHash)

            resultHash ← hasher(concatHashes)
              .leftMap(e ⇒ InternalVmError(s"Getting VM state for module=$instance failed", Some(e)): GetVmStateError)

          } yield resultHash
      }
      .map(ByteVector(_))

  // TODO : In the future, it should be rewritten with cats.effect.resource
  /**
    * Allocates memory in Wasm module of supplied size by allocateFunction.
    *
    * @param size size of memory that need to be allocated
    * @tparam F a monad with an ability to absorb 'IO'
    */
  private def allocate[F[_]: LiftIO: Monad](size: Int): EitherT[F, InvokeError, AnyRef] = {
    allocateFunction match {
      case Some(fn) => fn(size.asInstanceOf[AnyRef] :: Nil)
      case _ =>
        EitherT.leftT(
          NoSuchFnError(s"Unable to find the function for memory allocation with the name=$allocateFunctionName")
        )
    }
  }

  /**
    * Deallocates previously allocated memory in Wasm module by deallocateFunction.
    *
    * @param offset address of memory to deallocate
    * @tparam F a monad with an ability to absorb 'IO'
    */
  private def deallocate[F[_]: LiftIO: Monad](offset: Int): EitherT[F, InvokeError, AnyRef] = {
    deallocateFunction match {
      case Some(fn) => fn(offset.asInstanceOf[AnyRef] :: Nil)
      case _ =>
        EitherT.leftT(
          NoSuchFnError(s"Unable to find the function for memory deallocation with the name=$deallocateFunctionName")
        )
    }
  }

  /**
    * Preprocesses each string parameter: injects it into Wasm module memory (through
    * injectStringIntoWasmModule) and replace with pointer to it in WASM module and size.
    *
    * @param functionArguments arguments for calling this fn
    * @param moduleInstance module instance used for injecting string arguments to the Wasm memory
    * @tparam F a monad with an ability to absorb 'IO'
    */
  private def preprocessArguments[F[_]: LiftIO: Monad](
     functionArguments: Seq[String],
     moduleInstance: ModuleInstance
   ) : EitherT[F, InvokeError, List[String]] = {
    val result: Seq[EitherT[F, InvokeError, List[String]]] =
      functionArguments.map {
        case stringArgument if stringArgument.startsWith("\"")
            && stringArgument.endsWith("\"")
            && stringArgument.length >= 2 ⇒
          // it is a valid String parameter
          for {
            address <-
              injectStringIntoWasmModule(stringArgument.substring(1, stringArgument.length - 1), moduleInstance)
          } yield List(address.toString, (stringArgument.length - 2).toString)
        case arg ⇒ EitherT.rightT[F, InvokeError](List(arg))
      }

    import cats.instances.list._
    Traverse[List].flatSequence(result.toList)
   }

  /**
    * Injects given string into Wasm module memory.
    *
    * @param injectedString string that should be inserted into Wasm module memory
    * @param moduleInstance module instance used as a provider for Wasm module memory access
    * @tparam F a monad with an ability to absorb 'IO'
    */
  private def injectStringIntoWasmModule[F[_]: LiftIO: Monad](
    injectedString: String,
    moduleInstance: ModuleInstance
  ): EitherT[F, InvokeError, Int] =
    for {
      // In the current version, it is possible for Wasm module to have allocation/deallocation
      // functions but doesn't have memory (ByteBuffer instance). Also since it is used getMemory
      // Wasm function for getting ByteBuffer instance, it is also possible to don't have memory
      // due to possible absence of this function in the Wasm module.
      wasmMemory <- EitherT.fromOption(
        moduleInstance.memory,
        VmMemoryError(s"Trying to use absent Wasm memory while injecting string=$injectedString")
      )

      offset <- allocate(injectedString.length)

      resultOffset <- EitherT.fromEither(Try(offset.toString.toInt).toEither).leftMap(
        e ⇒ VmMemoryError(s"The Wasm allocation function returned incorrect offset=$offset", Some(e))
      )

      // It is possible for "evil" module to return knowingly invalid index (negative or doesn't
      // correspond to ByteBuffer limit)
      _ ← EitherT.cond(
        (resultOffset + injectedString.length) >= resultOffset &&
        // limit is the index of the first element that should not be read or written so
        // strictly less is needed
        (resultOffset + injectedString.length) < wasmMemory.limit(),
        (),
        VmMemoryError(
          s"String injecting failed due to invalid offset=$resultOffset"
        )
      )

      _ <- EitherT.rightT(
        injectedString.view.zipWithIndex.foreach {
          case (char, byteIdx) => wasmMemory.put(resultOffset + byteIdx, char.toByte)
        }
      )
    } yield resultOffset

}

object AsmleWasmVm {

  type WasmModules = List[ModuleInstance]
  type WasmFnIndex = Map[FunctionId, WasmFunction]

  /** Function id contains two components: optional module name and function name. */
  case class FunctionId(moduleName: Option[String], functionName: String) {
    override def toString =
      s"'${ModuleInstance.nameAsStr(moduleName)}.$functionName'"
  }

  /**
   * Representation for each Wasm function. Contains reference to module instance
   * and java method [[java.lang.reflect.Method]].
   *
   * @param functionId a full name of this function (moduleName + functionName)
   * @param javaMethod a java method [[java.lang.reflect.Method]] for calling fn.
   * @param module the object the underlying method is invoked from.
   *               This is an instance for the current module, it contains
   *               all inner state of the module, like memory.
   */
  case class WasmFunction(
    functionId: FunctionId,
    javaMethod: Method,
    module: ModuleInstance
  ) {

    /**
     * Invokes this function with arguments.
     *
     * @param args arguments for calling this fn.
     * @tparam F a monad with an ability to absorb 'IO'
     */
    def apply[F[_]: Functor: LiftIO](args: List[AnyRef]): EitherT[F, InvokeError, AnyRef] =
      EitherT(IO(javaMethod.invoke(module.instance, args: _*)).attempt.to[F])
        .leftMap(e ⇒ TrapError(s"Function $this with args: $args was failed", Some(e)))

    override def toString: String = functionId.toString
  }

  /** Transforms specified ''arg'' with ''castFn'', returns ''errorMsg'' otherwise. */
  private def cast[F[_]: Monad](
    arg: String,
    castFn: String ⇒ Any,
    errorMsg: String
  ): Either[InvalidArgError, AnyRef] =
    // it's important to cast to AnyRef type, because args should be used in
    // Method.invoke(...) as Object type
    Try(castFn(arg).asInstanceOf[AnyRef]).toEither.left
      .map(e ⇒ InvalidArgError(errorMsg, Some(e)))

  /** Safe array concatenation. Prevents array overflow or OOM. */
  private def safeConcat[F[_]: LiftIO: Monad](
    first: Array[Byte],
    second: Array[Byte]
  ): EitherT[F, InternalVmError, Array[Byte]] = {
    EitherT.fromEither {
      Try(Array.concat(second, first)).toEither
    }.leftMap(e ⇒ InternalVmError("Arrays concatenation failed", Some(e)))
  }

  private def parseArguments[F[_]: LiftIO: Monad](
    fnArgs: List[String],
    wasmFn: WasmFunction
  ): EitherT[F, InvalidArgError, List[AnyRef]] = {

    // Maps args to params
    val required = wasmFn.javaMethod.getParameterTypes.length

    for {
      _ ← EitherT.cond(
        required == fnArgs.size,
        (),
        InvalidArgError(
          s"Invalid number of arguments, expected=$required, actually=${fnArgs.size} for fn=$wasmFn " +
            s"(or passed string argument is incorrect or isn't matched the corresponding argument in Wasm function)"
        )
      )

      args = wasmFn.javaMethod.getParameterTypes.zipWithIndex.zip(fnArgs).map {
        case ((paramType, index), arg) =>
          paramType match {
            case cl if cl == classOf[Int] ⇒ cast(arg, _.toInt, s"Arg $index of '$arg' not an int " +
              s"(or passed string argument isn't matched the corresponding argument in Wasm function)")
            case cl if cl == classOf[Long] ⇒ cast(arg, _.toLong, s"Arg $index of '$arg' not a long " +
              s"(or passed string argument isn't matched the corresponding argument in Wasm function)")
            case cl if cl == classOf[Float] ⇒ cast(arg, _.toFloat, s"Arg $index of '$arg' not a float " +
              s"(or passed string argument isn't matched the corresponding argument in Wasm function)")
            case cl if cl == classOf[Double] ⇒ cast(arg, _.toDouble, s"Arg $index of '$arg' not a double " +
              s"(or passed string argument isn't matched the corresponding argument in Wasm function")
            case _ ⇒
              Left(
                InvalidArgError(
                  s"Invalid type for param $index: $paramType, expected [i32|i64|f32|f64]"
                )
              )
          }
      }

      result ← list2Either[F, InvalidArgError, AnyRef](args.toList)

    } yield result

  }

}