// GENERATED CODE - DO NOT MODIFY BY HAND
// coverage:ignore-file
// ignore_for_file: type=lint
// ignore_for_file: unused_element, deprecated_member_use, deprecated_member_use_from_same_package, use_function_type_syntax_for_parameters, unnecessary_const, avoid_init_to_null, invalid_override_different_default_values_named, prefer_expression_function_bodies, annotate_overrides, invalid_annotation_target, unnecessary_question_mark

part of 'api.dart';

// **************************************************************************
// FreezedGenerator
// **************************************************************************

// dart format off
T _$identity<T>(T value) => value;
/// @nodoc
mixin _$AgentEvent {





@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AgentEvent);
}


@override
int get hashCode => runtimeType.hashCode;

@override
String toString() {
  return 'AgentEvent()';
}


}

/// @nodoc
class $AgentEventCopyWith<$Res>  {
$AgentEventCopyWith(AgentEvent _, $Res Function(AgentEvent) __);
}


/// Adds pattern-matching-related methods to [AgentEvent].
extension AgentEventPatterns on AgentEvent {
/// A variant of `map` that fallback to returning `orElse`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeMap<TResult extends Object?>({TResult Function( AgentEvent_Connecting value)?  connecting,TResult Function( AgentEvent_Online value)?  online,TResult Function( AgentEvent_Recording value)?  recording,TResult Function( AgentEvent_RecordingStopped value)?  recordingStopped,TResult Function( AgentEvent_Reconnecting value)?  reconnecting,TResult Function( AgentEvent_Warn value)?  warn,TResult Function( AgentEvent_ParametersApplied value)?  parametersApplied,TResult Function( AgentEvent_Notice value)?  notice,TResult Function( AgentEvent_Stats value)?  stats,TResult Function( AgentEvent_Error value)?  error,required TResult orElse(),}){
final _that = this;
switch (_that) {
case AgentEvent_Connecting() when connecting != null:
return connecting(_that);case AgentEvent_Online() when online != null:
return online(_that);case AgentEvent_Recording() when recording != null:
return recording(_that);case AgentEvent_RecordingStopped() when recordingStopped != null:
return recordingStopped(_that);case AgentEvent_Reconnecting() when reconnecting != null:
return reconnecting(_that);case AgentEvent_Warn() when warn != null:
return warn(_that);case AgentEvent_ParametersApplied() when parametersApplied != null:
return parametersApplied(_that);case AgentEvent_Notice() when notice != null:
return notice(_that);case AgentEvent_Stats() when stats != null:
return stats(_that);case AgentEvent_Error() when error != null:
return error(_that);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// Callbacks receives the raw object, upcasted.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case final Subclass2 value:
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult map<TResult extends Object?>({required TResult Function( AgentEvent_Connecting value)  connecting,required TResult Function( AgentEvent_Online value)  online,required TResult Function( AgentEvent_Recording value)  recording,required TResult Function( AgentEvent_RecordingStopped value)  recordingStopped,required TResult Function( AgentEvent_Reconnecting value)  reconnecting,required TResult Function( AgentEvent_Warn value)  warn,required TResult Function( AgentEvent_ParametersApplied value)  parametersApplied,required TResult Function( AgentEvent_Notice value)  notice,required TResult Function( AgentEvent_Stats value)  stats,required TResult Function( AgentEvent_Error value)  error,}){
final _that = this;
switch (_that) {
case AgentEvent_Connecting():
return connecting(_that);case AgentEvent_Online():
return online(_that);case AgentEvent_Recording():
return recording(_that);case AgentEvent_RecordingStopped():
return recordingStopped(_that);case AgentEvent_Reconnecting():
return reconnecting(_that);case AgentEvent_Warn():
return warn(_that);case AgentEvent_ParametersApplied():
return parametersApplied(_that);case AgentEvent_Notice():
return notice(_that);case AgentEvent_Stats():
return stats(_that);case AgentEvent_Error():
return error(_that);}
}
/// A variant of `map` that fallback to returning `null`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? mapOrNull<TResult extends Object?>({TResult? Function( AgentEvent_Connecting value)?  connecting,TResult? Function( AgentEvent_Online value)?  online,TResult? Function( AgentEvent_Recording value)?  recording,TResult? Function( AgentEvent_RecordingStopped value)?  recordingStopped,TResult? Function( AgentEvent_Reconnecting value)?  reconnecting,TResult? Function( AgentEvent_Warn value)?  warn,TResult? Function( AgentEvent_ParametersApplied value)?  parametersApplied,TResult? Function( AgentEvent_Notice value)?  notice,TResult? Function( AgentEvent_Stats value)?  stats,TResult? Function( AgentEvent_Error value)?  error,}){
final _that = this;
switch (_that) {
case AgentEvent_Connecting() when connecting != null:
return connecting(_that);case AgentEvent_Online() when online != null:
return online(_that);case AgentEvent_Recording() when recording != null:
return recording(_that);case AgentEvent_RecordingStopped() when recordingStopped != null:
return recordingStopped(_that);case AgentEvent_Reconnecting() when reconnecting != null:
return reconnecting(_that);case AgentEvent_Warn() when warn != null:
return warn(_that);case AgentEvent_ParametersApplied() when parametersApplied != null:
return parametersApplied(_that);case AgentEvent_Notice() when notice != null:
return notice(_that);case AgentEvent_Stats() when stats != null:
return stats(_that);case AgentEvent_Error() when error != null:
return error(_that);case _:
  return null;

}
}
/// A variant of `when` that fallback to an `orElse` callback.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeWhen<TResult extends Object?>({TResult Function()?  connecting,TResult Function()?  online,TResult Function( String streamId)?  recording,TResult Function()?  recordingStopped,TResult Function( int attempt)?  reconnecting,TResult Function( String message)?  warn,TResult Function( String resolution,  int fps,  String codec,  int bitrateKbps)?  parametersApplied,TResult Function( String message)?  notice,TResult Function( BigInt sentFrames,  BigInt droppedFrames)?  stats,TResult Function( String message)?  error,required TResult orElse(),}) {final _that = this;
switch (_that) {
case AgentEvent_Connecting() when connecting != null:
return connecting();case AgentEvent_Online() when online != null:
return online();case AgentEvent_Recording() when recording != null:
return recording(_that.streamId);case AgentEvent_RecordingStopped() when recordingStopped != null:
return recordingStopped();case AgentEvent_Reconnecting() when reconnecting != null:
return reconnecting(_that.attempt);case AgentEvent_Warn() when warn != null:
return warn(_that.message);case AgentEvent_ParametersApplied() when parametersApplied != null:
return parametersApplied(_that.resolution,_that.fps,_that.codec,_that.bitrateKbps);case AgentEvent_Notice() when notice != null:
return notice(_that.message);case AgentEvent_Stats() when stats != null:
return stats(_that.sentFrames,_that.droppedFrames);case AgentEvent_Error() when error != null:
return error(_that.message);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// As opposed to `map`, this offers destructuring.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case Subclass2(:final field2):
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult when<TResult extends Object?>({required TResult Function()  connecting,required TResult Function()  online,required TResult Function( String streamId)  recording,required TResult Function()  recordingStopped,required TResult Function( int attempt)  reconnecting,required TResult Function( String message)  warn,required TResult Function( String resolution,  int fps,  String codec,  int bitrateKbps)  parametersApplied,required TResult Function( String message)  notice,required TResult Function( BigInt sentFrames,  BigInt droppedFrames)  stats,required TResult Function( String message)  error,}) {final _that = this;
switch (_that) {
case AgentEvent_Connecting():
return connecting();case AgentEvent_Online():
return online();case AgentEvent_Recording():
return recording(_that.streamId);case AgentEvent_RecordingStopped():
return recordingStopped();case AgentEvent_Reconnecting():
return reconnecting(_that.attempt);case AgentEvent_Warn():
return warn(_that.message);case AgentEvent_ParametersApplied():
return parametersApplied(_that.resolution,_that.fps,_that.codec,_that.bitrateKbps);case AgentEvent_Notice():
return notice(_that.message);case AgentEvent_Stats():
return stats(_that.sentFrames,_that.droppedFrames);case AgentEvent_Error():
return error(_that.message);}
}
/// A variant of `when` that fallback to returning `null`
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? whenOrNull<TResult extends Object?>({TResult? Function()?  connecting,TResult? Function()?  online,TResult? Function( String streamId)?  recording,TResult? Function()?  recordingStopped,TResult? Function( int attempt)?  reconnecting,TResult? Function( String message)?  warn,TResult? Function( String resolution,  int fps,  String codec,  int bitrateKbps)?  parametersApplied,TResult? Function( String message)?  notice,TResult? Function( BigInt sentFrames,  BigInt droppedFrames)?  stats,TResult? Function( String message)?  error,}) {final _that = this;
switch (_that) {
case AgentEvent_Connecting() when connecting != null:
return connecting();case AgentEvent_Online() when online != null:
return online();case AgentEvent_Recording() when recording != null:
return recording(_that.streamId);case AgentEvent_RecordingStopped() when recordingStopped != null:
return recordingStopped();case AgentEvent_Reconnecting() when reconnecting != null:
return reconnecting(_that.attempt);case AgentEvent_Warn() when warn != null:
return warn(_that.message);case AgentEvent_ParametersApplied() when parametersApplied != null:
return parametersApplied(_that.resolution,_that.fps,_that.codec,_that.bitrateKbps);case AgentEvent_Notice() when notice != null:
return notice(_that.message);case AgentEvent_Stats() when stats != null:
return stats(_that.sentFrames,_that.droppedFrames);case AgentEvent_Error() when error != null:
return error(_that.message);case _:
  return null;

}
}

}

/// @nodoc


class AgentEvent_Connecting extends AgentEvent {
  const AgentEvent_Connecting(): super._();
  






@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AgentEvent_Connecting);
}


@override
int get hashCode => runtimeType.hashCode;

@override
String toString() {
  return 'AgentEvent.connecting()';
}


}




/// @nodoc


class AgentEvent_Online extends AgentEvent {
  const AgentEvent_Online(): super._();
  






@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AgentEvent_Online);
}


@override
int get hashCode => runtimeType.hashCode;

@override
String toString() {
  return 'AgentEvent.online()';
}


}




/// @nodoc


class AgentEvent_Recording extends AgentEvent {
  const AgentEvent_Recording({required this.streamId}): super._();
  

/// Stream the server announced.
 final  String streamId;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$AgentEvent_RecordingCopyWith<AgentEvent_Recording> get copyWith => _$AgentEvent_RecordingCopyWithImpl<AgentEvent_Recording>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AgentEvent_Recording&&(identical(other.streamId, streamId) || other.streamId == streamId));
}


@override
int get hashCode => Object.hash(runtimeType,streamId);

@override
String toString() {
  return 'AgentEvent.recording(streamId: $streamId)';
}


}

/// @nodoc
abstract mixin class $AgentEvent_RecordingCopyWith<$Res> implements $AgentEventCopyWith<$Res> {
  factory $AgentEvent_RecordingCopyWith(AgentEvent_Recording value, $Res Function(AgentEvent_Recording) _then) = _$AgentEvent_RecordingCopyWithImpl;
@useResult
$Res call({
 String streamId
});




}
/// @nodoc
class _$AgentEvent_RecordingCopyWithImpl<$Res>
    implements $AgentEvent_RecordingCopyWith<$Res> {
  _$AgentEvent_RecordingCopyWithImpl(this._self, this._then);

  final AgentEvent_Recording _self;
  final $Res Function(AgentEvent_Recording) _then;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? streamId = null,}) {
  return _then(AgentEvent_Recording(
streamId: null == streamId ? _self.streamId : streamId // ignore: cast_nullable_to_non_nullable
as String,
  ));
}


}

/// @nodoc


class AgentEvent_RecordingStopped extends AgentEvent {
  const AgentEvent_RecordingStopped(): super._();
  






@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AgentEvent_RecordingStopped);
}


@override
int get hashCode => runtimeType.hashCode;

@override
String toString() {
  return 'AgentEvent.recordingStopped()';
}


}




/// @nodoc


class AgentEvent_Reconnecting extends AgentEvent {
  const AgentEvent_Reconnecting({required this.attempt}): super._();
  

/// Reconnect attempts since the last attach that worked, counting from one.
 final  int attempt;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$AgentEvent_ReconnectingCopyWith<AgentEvent_Reconnecting> get copyWith => _$AgentEvent_ReconnectingCopyWithImpl<AgentEvent_Reconnecting>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AgentEvent_Reconnecting&&(identical(other.attempt, attempt) || other.attempt == attempt));
}


@override
int get hashCode => Object.hash(runtimeType,attempt);

@override
String toString() {
  return 'AgentEvent.reconnecting(attempt: $attempt)';
}


}

/// @nodoc
abstract mixin class $AgentEvent_ReconnectingCopyWith<$Res> implements $AgentEventCopyWith<$Res> {
  factory $AgentEvent_ReconnectingCopyWith(AgentEvent_Reconnecting value, $Res Function(AgentEvent_Reconnecting) _then) = _$AgentEvent_ReconnectingCopyWithImpl;
@useResult
$Res call({
 int attempt
});




}
/// @nodoc
class _$AgentEvent_ReconnectingCopyWithImpl<$Res>
    implements $AgentEvent_ReconnectingCopyWith<$Res> {
  _$AgentEvent_ReconnectingCopyWithImpl(this._self, this._then);

  final AgentEvent_Reconnecting _self;
  final $Res Function(AgentEvent_Reconnecting) _then;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? attempt = null,}) {
  return _then(AgentEvent_Reconnecting(
attempt: null == attempt ? _self.attempt : attempt // ignore: cast_nullable_to_non_nullable
as int,
  ));
}


}

/// @nodoc


class AgentEvent_Warn extends AgentEvent {
  const AgentEvent_Warn({required this.message}): super._();
  

/// Human-readable message.
 final  String message;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$AgentEvent_WarnCopyWith<AgentEvent_Warn> get copyWith => _$AgentEvent_WarnCopyWithImpl<AgentEvent_Warn>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AgentEvent_Warn&&(identical(other.message, message) || other.message == message));
}


@override
int get hashCode => Object.hash(runtimeType,message);

@override
String toString() {
  return 'AgentEvent.warn(message: $message)';
}


}

/// @nodoc
abstract mixin class $AgentEvent_WarnCopyWith<$Res> implements $AgentEventCopyWith<$Res> {
  factory $AgentEvent_WarnCopyWith(AgentEvent_Warn value, $Res Function(AgentEvent_Warn) _then) = _$AgentEvent_WarnCopyWithImpl;
@useResult
$Res call({
 String message
});




}
/// @nodoc
class _$AgentEvent_WarnCopyWithImpl<$Res>
    implements $AgentEvent_WarnCopyWith<$Res> {
  _$AgentEvent_WarnCopyWithImpl(this._self, this._then);

  final AgentEvent_Warn _self;
  final $Res Function(AgentEvent_Warn) _then;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? message = null,}) {
  return _then(AgentEvent_Warn(
message: null == message ? _self.message : message // ignore: cast_nullable_to_non_nullable
as String,
  ));
}


}

/// @nodoc


class AgentEvent_ParametersApplied extends AgentEvent {
  const AgentEvent_ParametersApplied({required this.resolution, required this.fps, required this.codec, required this.bitrateKbps}): super._();
  

/// Protocol resolution, for example `1920x1080`.
 final  String resolution;
/// Frame rate in frames per second.
 final  int fps;
/// Codec the frames are encoded with, spelled as the protocol spells it (`h264`).
 final  String codec;
/// Encoder bitrate in kilobits per second.
 final  int bitrateKbps;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$AgentEvent_ParametersAppliedCopyWith<AgentEvent_ParametersApplied> get copyWith => _$AgentEvent_ParametersAppliedCopyWithImpl<AgentEvent_ParametersApplied>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AgentEvent_ParametersApplied&&(identical(other.resolution, resolution) || other.resolution == resolution)&&(identical(other.fps, fps) || other.fps == fps)&&(identical(other.codec, codec) || other.codec == codec)&&(identical(other.bitrateKbps, bitrateKbps) || other.bitrateKbps == bitrateKbps));
}


@override
int get hashCode => Object.hash(runtimeType,resolution,fps,codec,bitrateKbps);

@override
String toString() {
  return 'AgentEvent.parametersApplied(resolution: $resolution, fps: $fps, codec: $codec, bitrateKbps: $bitrateKbps)';
}


}

/// @nodoc
abstract mixin class $AgentEvent_ParametersAppliedCopyWith<$Res> implements $AgentEventCopyWith<$Res> {
  factory $AgentEvent_ParametersAppliedCopyWith(AgentEvent_ParametersApplied value, $Res Function(AgentEvent_ParametersApplied) _then) = _$AgentEvent_ParametersAppliedCopyWithImpl;
@useResult
$Res call({
 String resolution, int fps, String codec, int bitrateKbps
});




}
/// @nodoc
class _$AgentEvent_ParametersAppliedCopyWithImpl<$Res>
    implements $AgentEvent_ParametersAppliedCopyWith<$Res> {
  _$AgentEvent_ParametersAppliedCopyWithImpl(this._self, this._then);

  final AgentEvent_ParametersApplied _self;
  final $Res Function(AgentEvent_ParametersApplied) _then;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? resolution = null,Object? fps = null,Object? codec = null,Object? bitrateKbps = null,}) {
  return _then(AgentEvent_ParametersApplied(
resolution: null == resolution ? _self.resolution : resolution // ignore: cast_nullable_to_non_nullable
as String,fps: null == fps ? _self.fps : fps // ignore: cast_nullable_to_non_nullable
as int,codec: null == codec ? _self.codec : codec // ignore: cast_nullable_to_non_nullable
as String,bitrateKbps: null == bitrateKbps ? _self.bitrateKbps : bitrateKbps // ignore: cast_nullable_to_non_nullable
as int,
  ));
}


}

/// @nodoc


class AgentEvent_Notice extends AgentEvent {
  const AgentEvent_Notice({required this.message}): super._();
  

/// Human-readable message.
 final  String message;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$AgentEvent_NoticeCopyWith<AgentEvent_Notice> get copyWith => _$AgentEvent_NoticeCopyWithImpl<AgentEvent_Notice>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AgentEvent_Notice&&(identical(other.message, message) || other.message == message));
}


@override
int get hashCode => Object.hash(runtimeType,message);

@override
String toString() {
  return 'AgentEvent.notice(message: $message)';
}


}

/// @nodoc
abstract mixin class $AgentEvent_NoticeCopyWith<$Res> implements $AgentEventCopyWith<$Res> {
  factory $AgentEvent_NoticeCopyWith(AgentEvent_Notice value, $Res Function(AgentEvent_Notice) _then) = _$AgentEvent_NoticeCopyWithImpl;
@useResult
$Res call({
 String message
});




}
/// @nodoc
class _$AgentEvent_NoticeCopyWithImpl<$Res>
    implements $AgentEvent_NoticeCopyWith<$Res> {
  _$AgentEvent_NoticeCopyWithImpl(this._self, this._then);

  final AgentEvent_Notice _self;
  final $Res Function(AgentEvent_Notice) _then;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? message = null,}) {
  return _then(AgentEvent_Notice(
message: null == message ? _self.message : message // ignore: cast_nullable_to_non_nullable
as String,
  ));
}


}

/// @nodoc


class AgentEvent_Stats extends AgentEvent {
  const AgentEvent_Stats({required this.sentFrames, required this.droppedFrames}): super._();
  

/// Encoded frames handed to the core since the agent started.
 final  BigInt sentFrames;
/// Frames dropped, which happens inside the core's own media queue.
 final  BigInt droppedFrames;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$AgentEvent_StatsCopyWith<AgentEvent_Stats> get copyWith => _$AgentEvent_StatsCopyWithImpl<AgentEvent_Stats>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AgentEvent_Stats&&(identical(other.sentFrames, sentFrames) || other.sentFrames == sentFrames)&&(identical(other.droppedFrames, droppedFrames) || other.droppedFrames == droppedFrames));
}


@override
int get hashCode => Object.hash(runtimeType,sentFrames,droppedFrames);

@override
String toString() {
  return 'AgentEvent.stats(sentFrames: $sentFrames, droppedFrames: $droppedFrames)';
}


}

/// @nodoc
abstract mixin class $AgentEvent_StatsCopyWith<$Res> implements $AgentEventCopyWith<$Res> {
  factory $AgentEvent_StatsCopyWith(AgentEvent_Stats value, $Res Function(AgentEvent_Stats) _then) = _$AgentEvent_StatsCopyWithImpl;
@useResult
$Res call({
 BigInt sentFrames, BigInt droppedFrames
});




}
/// @nodoc
class _$AgentEvent_StatsCopyWithImpl<$Res>
    implements $AgentEvent_StatsCopyWith<$Res> {
  _$AgentEvent_StatsCopyWithImpl(this._self, this._then);

  final AgentEvent_Stats _self;
  final $Res Function(AgentEvent_Stats) _then;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? sentFrames = null,Object? droppedFrames = null,}) {
  return _then(AgentEvent_Stats(
sentFrames: null == sentFrames ? _self.sentFrames : sentFrames // ignore: cast_nullable_to_non_nullable
as BigInt,droppedFrames: null == droppedFrames ? _self.droppedFrames : droppedFrames // ignore: cast_nullable_to_non_nullable
as BigInt,
  ));
}


}

/// @nodoc


class AgentEvent_Error extends AgentEvent {
  const AgentEvent_Error({required this.message}): super._();
  

/// Human-readable reason.
 final  String message;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$AgentEvent_ErrorCopyWith<AgentEvent_Error> get copyWith => _$AgentEvent_ErrorCopyWithImpl<AgentEvent_Error>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AgentEvent_Error&&(identical(other.message, message) || other.message == message));
}


@override
int get hashCode => Object.hash(runtimeType,message);

@override
String toString() {
  return 'AgentEvent.error(message: $message)';
}


}

/// @nodoc
abstract mixin class $AgentEvent_ErrorCopyWith<$Res> implements $AgentEventCopyWith<$Res> {
  factory $AgentEvent_ErrorCopyWith(AgentEvent_Error value, $Res Function(AgentEvent_Error) _then) = _$AgentEvent_ErrorCopyWithImpl;
@useResult
$Res call({
 String message
});




}
/// @nodoc
class _$AgentEvent_ErrorCopyWithImpl<$Res>
    implements $AgentEvent_ErrorCopyWith<$Res> {
  _$AgentEvent_ErrorCopyWithImpl(this._self, this._then);

  final AgentEvent_Error _self;
  final $Res Function(AgentEvent_Error) _then;

/// Create a copy of AgentEvent
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') $Res call({Object? message = null,}) {
  return _then(AgentEvent_Error(
message: null == message ? _self.message : message // ignore: cast_nullable_to_non_nullable
as String,
  ));
}


}

// dart format on
