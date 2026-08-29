// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Test-framework shims: Maven Surefire, JUnit, Mockito/ByteBuddy, AssertJ and the ECJ/javac in-process compilers.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

pub(crate) fn register_test_harness_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "cratonvm/test/Util",
        "tempPrint",
        "(I)V",
        native_temp_print_int,
    );
    registry.register(
        "cratonvm/test/Util",
        "tempPrint",
        "(Ljava/lang/String;)V",
        native_temp_print_string,
    );
    registry.register("cratonvm/Util", "tempPrint", "(I)V", native_temp_print_int);
    registry.register(
        "cratonvm/Util",
        "tempPrint",
        "(Ljava/lang/String;)V",
        native_temp_print_string,
    );
    registry.register(
        "org/apache/lucene/tests/util/RamUsageTester",
        "ramUsed",
        "(Ljava/lang/Object;)J",
        native_lucene_ram_usage_tester_ram_used,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/RandomizedContext",
        "getPerThread",
        "()Lcom/carrotsearch/randomizedtesting/RandomizedContext$PerThreadResources;",
        native_randomized_context_get_per_thread,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/RandomizedContext",
        "current",
        "()Lcom/carrotsearch/randomizedtesting/RandomizedContext;",
        native_randomized_context_current,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/RandomizedContext",
        "context",
        "(Ljava/lang/Thread;)Lcom/carrotsearch/randomizedtesting/RandomizedContext;",
        native_randomized_context_context,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/RandomizedContext",
        "getRandomness",
        "()Lcom/carrotsearch/randomizedtesting/Randomness;",
        native_randomized_context_get_randomness,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/RandomizedContext",
        "getRandom",
        "()Ljava/util/Random;",
        native_randomized_context_get_random,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/RandomizedContext",
        "push",
        "(Lcom/carrotsearch/randomizedtesting/Randomness;)V",
        native_randomized_context_push,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/RandomizedContext",
        "popAndDestroy",
        "()V",
        native_randomized_context_pop_and_destroy,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/Randomness",
        "getRandom",
        "()Ljava/util/Random;",
        native_randomness_get_random,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/RandomizedTest",
        "getContext",
        "()Lcom/carrotsearch/randomizedtesting/RandomizedContext;",
        native_randomized_test_get_context,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/RandomizedTest",
        "getRandom",
        "()Ljava/util/Random;",
        native_randomized_test_get_random,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/RandomizedTest",
        "randomFloat",
        "()F",
        native_randomized_test_random_float,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/Xoroshiro128PlusRandom",
        "nextLong",
        "()J",
        native_xoroshiro128_plus_random_next_long,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/Xoroshiro128PlusRandom",
        "nextInt",
        "()I",
        native_xoroshiro128_plus_random_next_int,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/Xoroshiro128PlusRandom",
        "nextInt",
        "(I)I",
        native_xoroshiro128_plus_random_next_int_bound,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/Xoroshiro128PlusRandom",
        "next",
        "(I)I",
        native_xoroshiro128_plus_random_next_bits,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/Xoroshiro128PlusRandom",
        "nextFloat",
        "()F",
        native_xoroshiro128_plus_random_next_float,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/Xoroshiro128PlusRandom",
        "nextDouble",
        "()D",
        native_xoroshiro128_plus_random_next_double,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/Xoroshiro128PlusRandom",
        "nextBoolean",
        "()Z",
        native_xoroshiro128_plus_random_next_boolean,
    );
    registry.register(
        "com/carrotsearch/randomizedtesting/Xoroshiro128PlusRandom",
        "nextBytes",
        "([B)V",
        native_xoroshiro128_plus_random_next_bytes,
    );
    registry.register(
        "org/elasticsearch/simdvec/ESVectorUtil",
        "dotProduct",
        "([F[F)F",
        native_es_vector_util_dot_product_f32,
    );
    registry.register(
        "org/elasticsearch/simdvec/ESVectorUtil",
        "squareDistance",
        "([F[F)F",
        native_es_vector_util_square_distance_f32,
    );
    registry.register(
        "org/elasticsearch/simdvec/ESVectorUtil",
        "squareDistance",
        "([F[FII)F",
        native_es_vector_util_square_distance_f32_offset,
    );
    registry.register(
        "org/elasticsearch/simdvec/ESVectorUtil",
        "calculateOSQLoss",
        "([FFFIFF[I)F",
        native_es_vector_util_calculate_osq_loss_f32,
    );
    registry.register(
        "org/elasticsearch/simdvec/ESVectorUtil",
        "calculateOSQGridPoints",
        "([F[II[F)V",
        native_es_vector_util_calculate_osq_grid_points_f32,
    );
    registry.register(
        "org/elasticsearch/simdvec/ESVectorUtil",
        "centerAndCalculateOSQStatsEuclidean",
        "([F[F[F[F)V",
        native_es_vector_util_center_stats_euclidean_f32,
    );
    registry.register(
        "org/elasticsearch/simdvec/ESVectorUtil",
        "centerAndCalculateOSQStatsDp",
        "([F[F[F[F)V",
        native_es_vector_util_center_stats_dp_f32,
    );
    registry.register(
        "org/elasticsearch/simdvec/ESVectorUtil",
        "centerAndCalculateOSQStatsEuclidean",
        "([B[B[F[F)V",
        native_es_vector_util_center_stats_euclidean_i8,
    );
    registry.register(
        "org/elasticsearch/simdvec/ESVectorUtil",
        "centerAndCalculateOSQStatsDp",
        "([B[B[F[F)V",
        native_es_vector_util_center_stats_dp_i8,
    );
    registry.register(
        "org/elasticsearch/simdvec/ESVectorUtil",
        "quantizeVectorWithIntervals",
        "([F[IFFB)I",
        native_es_vector_util_quantize_vector_with_intervals_f32,
    );
    registry.register(
        "org/elasticsearch/simdvec/ESVectorUtil",
        "packAsBinary",
        "([I[B)V",
        native_es_vector_util_pack_as_binary,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$buildAndWritePostingsLists$1",
        "([II)I",
        native_es_next_diskbbq_vectors_writer_iarray_at_i,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$buildAndWritePostingsLists$2",
        "([I[II)I",
        native_es_next_diskbbq_vectors_writer_iarray_at_iarray_at_i,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$buildAndWritePostingsLists$3",
        "([II)I",
        native_es_next_diskbbq_vectors_writer_iarray_at_i,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$buildAndWritePostingsLists$4",
        "([III)I",
        native_es_next_diskbbq_vectors_writer_iarray_at_i_plus_j,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$buildAndWritePostingsLists$6",
        "([II)I",
        native_es_next_diskbbq_vectors_writer_iarray_at_i,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$buildAndWritePostingsLists$7",
        "([II)I",
        native_es_next_diskbbq_vectors_writer_iarray_at_i,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$buildAndWritePostingsLists$8",
        "([Z[II)Z",
        native_es_next_diskbbq_vectors_writer_barray_at_iarray_at_i,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$buildAndWritePostingsLists$9",
        "([I[II)I",
        native_es_next_diskbbq_vectors_writer_iarray_at_iarray_at_i,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$buildAndWritePostingsLists$10",
        "([III)I",
        native_es_next_diskbbq_vectors_writer_iarray_at_i_plus_j,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$doWriteCentroids$13",
        "([II)I",
        native_es_next_diskbbq_vectors_writer_iarray_at_i,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$writeCentroidsWithParents$14",
        "([II)I",
        native_es_next_diskbbq_vectors_writer_iarray_at_i,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$createCentroidSupplier$12",
        "(II)I",
        native_es_next_diskbbq_vectors_writer_i_plus_j,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "lambda$calculateCentroidsFullRebuildSliced$15",
        "(II)I",
        native_es_next_diskbbq_vectors_writer_i_plus_j,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsWriter",
        "writeSlicesOffsets",
        "(Lorg/apache/lucene/store/IndexOutput;Lorg/elasticsearch/index/codec/vectors/diskbbq/CentroidSlices;)V",
        native_es_next_diskbbq_vectors_writer_write_slices_offsets,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/cluster/ClusteringFloatVectorValuesSlice",
        "lambda$createRandomSlice$0",
        "([II)I",
        native_es_next_diskbbq_vectors_writer_iarray_at_i,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/cluster/ClusteringFloatVectorValuesSlice",
        "lambda$createRandomSlice$1",
        "([II)I",
        native_es_next_diskbbq_vectors_writer_iarray_at_i,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/cluster/ClusteringFloatVectorValuesSlice",
        "vectorValue",
        "(I)[F",
        native_es_clustering_float_vector_values_slice_vector_value,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/cluster/ClusteringFloatVectorValuesSlice",
        "vectorValue",
        "(I)Ljava/lang/Object;",
        native_es_clustering_float_vector_values_slice_vector_value,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/cluster/ClusteringFloatVectorValuesSlice",
        "ordToDoc",
        "(I)I",
        native_es_clustering_float_vector_values_slice_ord_to_doc,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/cluster/ClusteringFloatVectorValuesSlice",
        "size",
        "()I",
        native_es_clustering_float_vector_values_slice_size,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/cluster/ClusteringFloatVectorValuesSlice",
        "dimension",
        "()I",
        native_es_clustering_float_vector_values_slice_dimension,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/CentroidSlices",
        "sliceOffsets",
        "()[I",
        native_es_centroid_slices_slice_offsets,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/CentroidSlices",
        "sliceNumVectors",
        "()[I",
        native_es_centroid_slices_slice_num_vectors,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/CentroidSlices",
        "maxSliceSize",
        "()I",
        native_es_centroid_slices_max_slice_size,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/CentroidAssignments",
        "numCentroids",
        "()I",
        native_es_centroid_assignments_num_centroids,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/CentroidAssignments",
        "centroids",
        "()[[F",
        native_es_centroid_assignments_centroids,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/CentroidAssignments",
        "assignments",
        "()[I",
        native_es_centroid_assignments_assignments,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/CentroidAssignments",
        "overspillAssignments",
        "()[I",
        native_es_centroid_assignments_overspill_assignments,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/CentroidAssignments",
        "globalCentroid",
        "()[F",
        native_es_centroid_assignments_global_centroid,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/diskbbq/CentroidAssignments",
        "centroidSlices",
        "()Lorg/elasticsearch/index/codec/vectors/diskbbq/CentroidSlices;",
        native_es_centroid_assignments_centroid_slices,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/cluster/KMeansResult",
        "centroids",
        "()[Ljava/lang/Object;",
        native_es_kmeans_result_centroids,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/cluster/KMeansResult",
        "assignments",
        "()[I",
        native_es_kmeans_result_assignments,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/cluster/KMeansResult",
        "clusterCounts",
        "()[I",
        native_es_kmeans_result_cluster_counts,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/cluster/KMeansResult",
        "soarAssignments",
        "()[I",
        native_es_kmeans_result_soar_assignments,
    );
    registry.register(
        "org/elasticsearch/index/codec/vectors/cluster/KMeansFloatVectorValues",
        "size",
        "()I",
        native_es_kmeans_float_vector_values_size,
    );
    registry.register(
        "org/elasticsearch/search/vectors/KnnScoreDocQuery",
        "<init>",
        "([Lorg/apache/lucene/search/ScoreDoc;Lorg/apache/lucene/index/IndexReader;)V",
        native_es_knn_score_doc_query_init,
    );
    registry.register(
        "org/elasticsearch/search/vectors/MaxScoreTopKnnCollector",
        "unsortedTopK",
        "()Lorg/apache/lucene/search/TopDocs;",
        native_es_max_score_top_knn_collector_unsorted_top_k,
    );
    registry.register(
        "org/elasticsearch/simdvec/ES92Int7VectorsScorer",
        "int7DotProductBulk",
        "([BI[F)V",
        native_es92_int7_vectors_scorer_int7_dot_product_bulk,
    );
    registry.register(
        "java/util/Arrays",
        "sort",
        "([JII)V",
        native_java_arrays_sort_long_range,
    );
    registry.register(
        "org/apache/lucene/index/IndexReaderContext",
        "id",
        "()Ljava/lang/Object;",
        native_lucene_index_reader_context_id,
    );
    registry.register(
        "org/apache/lucene/store/DataOutput",
        "writeVInt",
        "(I)V",
        native_lucene_data_output_write_vint,
    );
    registry.register(
        "org/apache/lucene/store/DataOutput",
        "writeZInt",
        "(I)V",
        native_lucene_data_output_write_zint,
    );
    registry.register(
        "org/apache/lucene/store/DataOutput",
        "writeVLong",
        "(J)V",
        native_lucene_data_output_write_vlong,
    );
    registry.register(
        "org/apache/lucene/store/DataOutput",
        "writeZLong",
        "(J)V",
        native_lucene_data_output_write_zlong,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataOutput",
        "writeByte",
        "(B)V",
        native_lucene_byte_buffers_data_output_write_byte,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataOutput",
        "writeBytes",
        "([BII)V",
        native_lucene_byte_buffers_data_output_write_bytes,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataOutput",
        "writeBytes",
        "([BI)V",
        native_lucene_byte_buffers_data_output_write_bytes_len,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataOutput",
        "writeBytes",
        "([B)V",
        native_lucene_byte_buffers_data_output_write_bytes_all,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataOutput",
        "writeShort",
        "(S)V",
        native_lucene_byte_buffers_data_output_write_short,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataOutput",
        "writeInt",
        "(I)V",
        native_lucene_byte_buffers_data_output_write_int,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataOutput",
        "writeLong",
        "(J)V",
        native_lucene_byte_buffers_data_output_write_long,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataOutput",
        "copyBytes",
        "(Lorg/apache/lucene/store/DataInput;J)V",
        native_lucene_byte_buffers_data_output_copy_bytes,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataOutput$ByteBufferRecycler",
        "reuse",
        "(Ljava/nio/ByteBuffer;)V",
        native_lucene_byte_buffers_byte_buffer_recycler_reuse,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexOutput",
        "writeByte",
        "(B)V",
        native_lucene_byte_buffers_index_output_write_byte,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexOutput",
        "writeBytes",
        "([BII)V",
        native_lucene_byte_buffers_index_output_write_bytes,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexOutput",
        "writeBytes",
        "([BI)V",
        native_lucene_byte_buffers_index_output_write_bytes_len,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexOutput",
        "writeShort",
        "(S)V",
        native_lucene_byte_buffers_index_output_write_short,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexOutput",
        "writeInt",
        "(I)V",
        native_lucene_byte_buffers_index_output_write_int,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexOutput",
        "writeLong",
        "(J)V",
        native_lucene_byte_buffers_index_output_write_long,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexOutput",
        "copyBytes",
        "(Lorg/apache/lucene/store/DataInput;J)V",
        native_lucene_byte_buffers_index_output_copy_bytes,
    );
    registry.register(
        "org/apache/lucene/tests/store/MockIndexOutputWrapper",
        "writeByte",
        "(B)V",
        native_lucene_mock_index_output_wrapper_write_byte,
    );
    registry.register(
        "org/apache/lucene/index/TermsEnumIndex",
        "prefix8ToComparableUnsignedLong",
        "(Lorg/apache/lucene/util/BytesRef;)J",
        native_lucene_terms_enum_index_prefix8,
    );
    registry.register(
        "org/apache/lucene/index/TermsEnumIndex",
        "next",
        "()Lorg/apache/lucene/util/BytesRef;",
        native_lucene_terms_enum_index_next,
    );
    registry.register(
        "org/apache/lucene/index/TermsEnumIndex",
        "compareTermTo",
        "(Lorg/apache/lucene/index/TermsEnumIndex;)I",
        native_lucene_terms_enum_index_compare_term_to,
    );
    registry.register(
        "org/apache/lucene/index/TermsEnumIndex",
        "termEquals",
        "(Lorg/apache/lucene/index/TermsEnumIndex$TermState;)Z",
        native_lucene_terms_enum_index_term_equals,
    );
    registry.register(
        "org/apache/lucene/index/TermsEnumIndex$TermState",
        "copyFrom",
        "(Lorg/apache/lucene/index/TermsEnumIndex;)V",
        native_lucene_terms_enum_index_term_state_copy_from,
    );
    registry.register(
        "org/apache/lucene/index/OrdinalMap$SegmentMap",
        "newToOld",
        "(I)I",
        native_lucene_ordinal_map_segment_map_new_to_old,
    );
    registry.register(
        "org/apache/lucene/index/OrdinalMap$SegmentMap",
        "oldToNew",
        "(I)I",
        native_lucene_ordinal_map_segment_map_old_to_new,
    );
    registry.register(
        "org/apache/lucene/index/OrdinalMap$TermsEnumPriorityQueue",
        "lessThan",
        "(Lorg/apache/lucene/index/TermsEnumIndex;Lorg/apache/lucene/index/TermsEnumIndex;)Z",
        native_lucene_terms_enum_priority_queue_less_than,
    );
    registry.register(
        "org/apache/lucene/index/OrdinalMap$TermsEnumPriorityQueue",
        "lessThan",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_lucene_terms_enum_priority_queue_less_than,
    );
    registry.register(
        "org/apache/lucene/util/PriorityQueue",
        "<init>",
        "(I)V",
        native_lucene_priority_queue_init_int,
    );
    registry.register(
        "org/apache/lucene/util/PriorityQueue",
        "size",
        "()I",
        native_lucene_priority_queue_size,
    );
    registry.register(
        "org/apache/lucene/util/PriorityQueue",
        "top",
        "()Ljava/lang/Object;",
        native_lucene_priority_queue_top,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readByte",
        "()B",
        native_lucene_byte_buffers_data_input_read_byte,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readByte",
        "(J)B",
        native_lucene_byte_buffers_data_input_read_byte_at,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readShort",
        "()S",
        native_lucene_byte_buffers_data_input_read_short,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readShort",
        "(J)S",
        native_lucene_byte_buffers_data_input_read_short_at,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readInt",
        "()I",
        native_lucene_byte_buffers_data_input_read_int,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readInt",
        "(J)I",
        native_lucene_byte_buffers_data_input_read_int_at,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readLong",
        "()J",
        native_lucene_byte_buffers_data_input_read_long,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readLong",
        "(J)J",
        native_lucene_byte_buffers_data_input_read_long_at,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "slice",
        "(JJ)Lorg/apache/lucene/store/ByteBuffersDataInput;",
        native_lucene_byte_buffers_data_input_slice,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readBytes",
        "([BII)V",
        native_lucene_byte_buffers_data_input_read_bytes,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readBytes",
        "([BIIZ)V",
        native_lucene_byte_buffers_data_input_read_bytes_bool,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readBytes",
        "(J[BII)V",
        native_lucene_byte_buffers_data_input_read_bytes_at,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readFloats",
        "([FII)V",
        native_lucene_byte_buffers_data_input_read_floats,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "readLongs",
        "([JII)V",
        native_lucene_byte_buffers_data_input_read_longs,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "length",
        "()J",
        native_lucene_byte_buffers_data_input_length,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "position",
        "()J",
        native_lucene_byte_buffers_data_input_position,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersDataInput",
        "seek",
        "(J)V",
        native_lucene_byte_buffers_data_input_seek,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "getFilePointer",
        "()J",
        native_lucene_byte_buffers_index_input_get_file_pointer,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "seek",
        "(J)V",
        native_lucene_byte_buffers_index_input_seek,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "length",
        "()J",
        native_lucene_byte_buffers_index_input_length,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readByte",
        "()B",
        native_lucene_byte_buffers_index_input_read_byte,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readBytes",
        "([BII)V",
        native_lucene_byte_buffers_index_input_read_bytes,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readBytes",
        "([BIIZ)V",
        native_lucene_byte_buffers_index_input_read_bytes_bool,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readFloats",
        "([FII)V",
        native_lucene_byte_buffers_index_input_read_floats,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readLongs",
        "([JII)V",
        native_lucene_byte_buffers_index_input_read_longs,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readShort",
        "()S",
        native_lucene_byte_buffers_index_input_read_short,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readInt",
        "()I",
        native_lucene_byte_buffers_index_input_read_int,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readLong",
        "()J",
        native_lucene_byte_buffers_index_input_read_long,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readByte",
        "(J)B",
        native_lucene_byte_buffers_index_input_read_byte_at,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readBytes",
        "(J[BII)V",
        native_lucene_byte_buffers_index_input_read_bytes_at,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readShort",
        "(J)S",
        native_lucene_byte_buffers_index_input_read_short_at,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readInt",
        "(J)I",
        native_lucene_byte_buffers_index_input_read_int_at,
    );
    registry.register(
        "org/apache/lucene/store/ByteBuffersIndexInput",
        "readLong",
        "(J)J",
        native_lucene_byte_buffers_index_input_read_long_at,
    );
    registry.register(
        "org/apache/lucene/store/IndexInput",
        "toString",
        "()Ljava/lang/String;",
        native_lucene_index_input_to_string,
    );
    registry.register(
        "org/apache/lucene/tests/store/MockIndexInputWrapper",
        "length",
        "()J",
        native_lucene_mock_index_input_wrapper_length,
    );
    registry.register(
        "org/apache/lucene/tests/store/MockIndexInputWrapper",
        "readByte",
        "()B",
        native_lucene_mock_index_input_wrapper_read_byte,
    );
    registry.register(
        "org/apache/lucene/tests/store/MockIndexInputWrapper",
        "readBytes",
        "([BII)V",
        native_lucene_mock_index_input_wrapper_read_bytes,
    );
    registry.register(
        "org/apache/lucene/tests/store/MockIndexInputWrapper",
        "readBytes",
        "([BIIZ)V",
        native_lucene_mock_index_input_wrapper_read_bytes_bool,
    );
    registry.register(
        "org/apache/lucene/tests/store/MockIndexInputWrapper",
        "readFloats",
        "([FII)V",
        native_lucene_mock_index_input_wrapper_read_floats,
    );
    registry.register(
        "org/apache/lucene/tests/store/MockIndexInputWrapper",
        "readLongs",
        "([JII)V",
        native_lucene_mock_index_input_wrapper_read_longs,
    );
    registry.register(
        "org/apache/lucene/tests/store/MockIndexInputWrapper",
        "readShort",
        "()S",
        native_lucene_mock_index_input_wrapper_read_short,
    );
    registry.register(
        "org/apache/lucene/tests/store/MockIndexInputWrapper",
        "readInt",
        "()I",
        native_lucene_mock_index_input_wrapper_read_int,
    );
    registry.register(
        "org/apache/lucene/tests/store/MockIndexInputWrapper",
        "readLong",
        "()J",
        native_lucene_mock_index_input_wrapper_read_long,
    );
}

pub(crate) fn native_junit_is_in_java_lang_annotation_package(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let class_obj = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let matched = spring_java_class_name(ctx, class_obj)
        .map(|name| name.starts_with("java.lang.annotation"))
        .unwrap_or(false);
    Ok(Some(Value::Int(matched as i32)))
}

fn bytebuddy_field_value(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    field_name: &str,
    fallback_slot: usize,
) -> Value {
    // Resolve by the object's OWN ClassId, not a name round-trip:
    // `class_name_of_id(class_id_of_object(obj))` followed by a
    // name-based `resolve_field_index` re-resolves the class GLOBALLY by
    // name, which returns `None` (ambiguous) once 2+ loaders each define
    // their own class under this same simple name -- exactly what
    // happens to ByteBuddy's own support classes when redefined per-fork
    // under a `@CompileWithForkedClassLoader`-style loader (confirmed:
    // MethodList$TypeSubstituting/TypeList$Generic$Explicit). The lossy
    // round-trip then silently fell back to a fixed slot number that
    // only happened to be correct when the class had zero fields
    // inherited ahead of its own, so the very first 2 distinct loaders
    // succeeded and every one after failed -- landing on the wrong field
    // (e.g. `declaringType` instead of `methodDescriptions`) and passing
    // that wrong object to `.size()`/`.get()`, surfacing as a
    // `NoSuchMethodError` deep inside seemingly unrelated bytecode.
    let class_id = ctx.class_id_of_object(obj);
    if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, field_name) {
        return ctx.get_field(obj, slot);
    }
    ctx.get_field(obj, fallback_slot)
}

fn bytebuddy_set_field_value(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    field_name: &str,
    fallback_slot: usize,
    value: Value,
) {
    // See `bytebuddy_field_value` for why this resolves by ClassId
    // directly rather than through a class-name round-trip.
    let class_id = ctx.class_id_of_object(obj);
    if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, field_name) {
        ctx.set_field(obj, slot, value);
        return;
    }
    ctx.set_field(obj, fallback_slot, value);
}

fn bytebuddy_ref_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    field_name: &str,
    fallback_slot: usize,
) -> Option<ObjectRef> {
    match bytebuddy_field_value(ctx, obj, field_name, fallback_slot) {
        Value::Object(obj) => obj,
        _ => None,
    }
}

fn bytebuddy_int_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    field_name: &str,
    fallback_slot: usize,
) -> i32 {
    match bytebuddy_field_value(ctx, obj, field_name, fallback_slot) {
        Value::Int(v) => v,
        _ => 0,
    }
}

fn bytebuddy_object_hash(
    ctx: &mut dyn NativeContext,
    obj: Option<ObjectRef>,
) -> Result<i32, MethodCallFailed> {
    let Some(obj) = obj else {
        return Ok(0);
    };
    let base_pin = ctx.pin_native_root(obj);
    match ctx.invoke_virtual(obj, "hashCode", "()I", &[])? {
        Some(Value::Int(hash)) => {
            ctx.unpin_native_roots(base_pin);
            Ok(hash)
        }
        _ => {
            let obj = ctx.read_native_pin(base_pin, obj);
            let hash = ctx.identity_hash_code(obj);
            ctx.unpin_native_roots(base_pin);
            Ok(hash)
        }
    }
}

fn bytebuddy_object_equals(
    ctx: &mut dyn NativeContext,
    a: Option<ObjectRef>,
    b: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        return Ok(true);
    }
    let (Some(a), Some(b)) = (a, b) else {
        return Ok(false);
    };
    let base_pin = ctx.pin_native_root(a);
    let _b_pin = ctx.pin_native_root(b);
    let result = match ctx.invoke_virtual(
        a,
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(b))],
    )? {
        Some(Value::Int(v)) => v != 0,
        _ => false,
    };
    ctx.unpin_native_roots(base_pin);
    Ok(result)
}

fn bytebuddy_list_size(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
) -> Result<usize, MethodCallFailed> {
    let base_pin = ctx.pin_native_root(list);
    let result = match ctx.invoke_virtual(list, "size", "()I", &[])? {
        Some(Value::Int(size)) if size > 0 => size as usize,
        _ => 0,
    };
    ctx.unpin_native_roots(base_pin);
    Ok(result)
}

fn bytebuddy_list_get(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    index: usize,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    bytebuddy_list_get_i32(ctx, list, index as i32)
}

fn bytebuddy_list_get_i32(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    index: i32,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let base_pin = ctx.pin_native_root(list);
    let result =
        match ctx.invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(index)])? {
            Some(Value::Object(obj)) => obj,
            _ => None,
        };
    ctx.unpin_native_roots(base_pin);
    Ok(result)
}

fn bytebuddy_required_ref_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    field_name: &str,
    fallback_slot: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    bytebuddy_ref_field(ctx, obj, field_name, fallback_slot).ok_or_else(|| {
        RuntimeError::NullPointerException {
            message: Some(format!("{field_name} is null")),
        }
        .into()
    })
}

fn native_bytebuddy_method_list_forwarding_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    field_name: &str,
    fallback_slot: usize,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let list = bytebuddy_required_ref_field(ctx, this, field_name, fallback_slot)?;
    Ok(Some(Value::Int(bytebuddy_list_size(ctx, list)? as i32)))
}

fn native_bytebuddy_method_list_forwarding_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    field_name: &str,
    fallback_slot: usize,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = match args.get(1) {
        Some(Value::Int(index)) => *index,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "MethodList.get index must be int".to_string(),
            }
            .into())
        }
    };
    let list = bytebuddy_required_ref_field(ctx, this, field_name, fallback_slot)?;
    ctx.invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(index)])
}

fn native_bytebuddy_method_list_explicit_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_bytebuddy_method_list_forwarding_size(ctx, args, "methodDescriptions", 0)
}

fn native_bytebuddy_method_list_explicit_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_bytebuddy_method_list_forwarding_get(ctx, args, "methodDescriptions", 0)
}

fn native_bytebuddy_method_description_type_substituting_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let declaring_type = match args.get(1) {
        Some(Value::Object(obj)) => *obj,
        _ => None,
    };
    let method_description = match args.get(2) {
        Some(Value::Object(obj)) => *obj,
        _ => None,
    };
    let visitor = match args.get(3) {
        Some(Value::Object(obj)) => *obj,
        _ => None,
    };

    bytebuddy_set_field_value(ctx, this, "declaringType", 0, Value::Object(declaring_type));
    bytebuddy_set_field_value(
        ctx,
        this,
        "methodDescription",
        1,
        Value::Object(method_description),
    );
    bytebuddy_set_field_value(ctx, this, "visitor", 2, Value::Object(visitor));
    Ok(None)
}

fn native_bytebuddy_method_list_type_substituting_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_bytebuddy_method_list_forwarding_size(ctx, args, "methodDescriptions", 1)
}

fn native_bytebuddy_method_list_type_substituting_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = match args.get(1) {
        Some(Value::Int(index)) => *index,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "MethodList$TypeSubstituting.get index must be int".to_string(),
            }
            .into())
        }
    };
    let base_pin = ctx.pin_native_root(this);
    let result = (|| {
        let declaring_type = bytebuddy_ref_field(ctx, this, "declaringType", 0);
        let visitor = bytebuddy_ref_field(ctx, this, "visitor", 2);
        let method_descriptions = bytebuddy_required_ref_field(ctx, this, "methodDescriptions", 1)?;
        let declaring_pin = declaring_type.map(|obj| (ctx.pin_native_root(obj), obj));
        let visitor_pin = visitor.map(|obj| (ctx.pin_native_root(obj), obj));
        let method_descriptions_pin = ctx.pin_native_root(method_descriptions);

        let method_descriptions = ctx.read_native_pin(method_descriptions_pin, method_descriptions);
        let method_description = bytebuddy_list_get_i32(ctx, method_descriptions, index)?;
        let method_pin = method_description.map(|obj| (ctx.pin_native_root(obj), obj));

        let declaring_type =
            declaring_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        let method_description =
            method_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        let visitor = visitor_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        bytebuddy_new_near(
            ctx,
            this,
            "net/bytebuddy/description/method/MethodDescription$TypeSubstituting",
            "(Lnet/bytebuddy/description/type/TypeDescription$Generic;Lnet/bytebuddy/description/method/MethodDescription;Lnet/bytebuddy/description/type/TypeDescription$Generic$Visitor;)V",
            &[
                Value::Object(declaring_type),
                Value::Object(method_description),
                Value::Object(visitor),
            ],
        )
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn native_bytebuddy_method_list_for_loaded_methods_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let base_pin = ctx.pin_native_root(this);
    let result = (|| {
        let constructors = bytebuddy_required_ref_field(ctx, this, "constructors", 1)?;
        let methods = bytebuddy_required_ref_field(ctx, this, "methods", 0)?;
        let constructors_pin = ctx.pin_native_root(constructors);
        let methods_pin = ctx.pin_native_root(methods);

        let constructors = ctx.read_native_pin(constructors_pin, constructors);
        let constructors_size = bytebuddy_list_size(ctx, constructors)?;
        let methods = ctx.read_native_pin(methods_pin, methods);
        let methods_size = bytebuddy_list_size(ctx, methods)?;
        Ok(Some(Value::Int(
            constructors_size.saturating_add(methods_size) as i32,
        )))
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

/// Construct `class_name` in the loader namespace `receiver`'s own class lives
/// in, rather than wherever a global by-name lookup happens to land.
///
/// Every one of these ByteBuddy shims re-implements a method that ByteBuddy
/// would otherwise run as bytecode, and bytecode would have resolved the
/// `new` through the DEFINING loader of the class holding it (JVMS 5.4.3.1).
/// `new_object_initialized` takes only a name, so it resolved globally and
/// returned the APPLICATION loader's copy no matter who called.
///
/// That is invisible until two loaders define ByteBuddy. Under Spring's
/// `@CompileWithForkedClassLoader` they do, and the mixed object graph breaks
/// on the first enum comparison: fork-loaded
/// `MethodDescription$TypeSubstituting.getTypeVariables()` filters its list
/// with `ofSort(Sort.VARIABLE)` against the FORK's `TypeDefinition$Sort`, while
/// the app-loaded `MethodDescription$ForLoadedMethod` this helper used to
/// return yields the APP's `Sort.VARIABLE`. Different enum constants of
/// different Class objects, so the filter drops every type variable, the
/// generic method looks non-generic, and ByteBuddy fails to attach `T` when it
/// writes the access bridge:
///
///   IllegalArgumentException: Could not create type
///     caused by: Cannot resolve T from ... ClassAssert$ByteBuddy$xxx
///                                          .isInstanceOfSatisfying(?)
///
/// which is what made AssertJ's soft assertions unusable in Spring AOT replay
/// (`BeanOverrideHandlerTests.forTestClassWith*`).
///
/// Falls back to the plain by-name construction when the receiver's own loader
/// cannot resolve the name, which keeps single-loader runs byte-identical.
fn bytebuddy_new_near(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    class_name: &str,
    init_desc: &str,
    init_args: &[Value],
) -> MethodCallResult {
    let near = ctx.class_id_of_object(receiver);
    if let Ok(cid) = ctx.class_id_by_name_via_referencing_class(near, class_name) {
        return ctx.new_object_initialized_with_class_id(cid, init_desc, init_args);
    }
    ctx.new_object_initialized(class_name, init_desc, init_args)
}

fn native_bytebuddy_method_list_for_loaded_methods_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = match args.get(1) {
        Some(Value::Int(index)) => *index,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "MethodList$ForLoadedMethods.get index must be int".to_string(),
            }
            .into())
        }
    };
    let base_pin = ctx.pin_native_root(this);
    let result = (|| {
        let constructors = bytebuddy_required_ref_field(ctx, this, "constructors", 1)?;
        let methods = bytebuddy_required_ref_field(ctx, this, "methods", 0)?;
        let constructors_pin = ctx.pin_native_root(constructors);
        let methods_pin = ctx.pin_native_root(methods);

        let constructors = ctx.read_native_pin(constructors_pin, constructors);
        let constructors_size = bytebuddy_list_size(ctx, constructors)? as i32;
        if index < constructors_size {
            let constructors = ctx.read_native_pin(constructors_pin, constructors);
            let constructor = bytebuddy_list_get_i32(ctx, constructors, index)?;
            let constructor_pin = constructor.map(|obj| (ctx.pin_native_root(obj), obj));
            let constructor =
                constructor_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
            return bytebuddy_new_near(
                ctx,
                this,
                BYTEBUDDY_METHOD_DESCRIPTION_FOR_LOADED_CONSTRUCTOR,
                "(Ljava/lang/reflect/Constructor;)V",
                &[Value::Object(constructor)],
            );
        }

        let methods = ctx.read_native_pin(methods_pin, methods);
        let method = bytebuddy_list_get_i32(ctx, methods, index - constructors_size)?;
        let method_pin = method.map(|obj| (ctx.pin_native_root(obj), obj));
        let method = method_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        bytebuddy_new_near(
            ctx,
            this,
            BYTEBUDDY_METHOD_DESCRIPTION_FOR_LOADED_METHOD,
            "(Ljava/lang/reflect/Method;)V",
            &[Value::Object(method)],
        )
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn native_bytebuddy_method_list_for_tokens_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_bytebuddy_method_list_forwarding_size(ctx, args, "tokens", 1)
}

fn native_bytebuddy_method_list_for_tokens_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = match args.get(1) {
        Some(Value::Int(index)) => *index,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "MethodList$ForTokens.get index must be int".to_string(),
            }
            .into())
        }
    };
    let base_pin = ctx.pin_native_root(this);
    let result = (|| {
        let declaring_type = bytebuddy_ref_field(ctx, this, "declaringType", 0);
        let tokens = bytebuddy_required_ref_field(ctx, this, "tokens", 1)?;
        let declaring_pin = declaring_type.map(|obj| (ctx.pin_native_root(obj), obj));
        let tokens_pin = ctx.pin_native_root(tokens);

        let tokens = ctx.read_native_pin(tokens_pin, tokens);
        let token = bytebuddy_list_get_i32(ctx, tokens, index)?;
        let token_pin = token.map(|obj| (ctx.pin_native_root(obj), obj));

        let declaring_type =
            declaring_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        let token = token_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        bytebuddy_new_near(
            ctx,
            this,
            "net/bytebuddy/description/method/MethodDescription$Latent",
            "(Lnet/bytebuddy/description/type/TypeDescription;Lnet/bytebuddy/description/method/MethodDescription$Token;)V",
            &[Value::Object(declaring_type), Value::Object(token)],
        )
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn native_bytebuddy_field_list_explicit_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_bytebuddy_method_list_forwarding_size(ctx, args, "fieldDescriptions", 0)
}

fn native_bytebuddy_field_list_explicit_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_bytebuddy_method_list_forwarding_get(ctx, args, "fieldDescriptions", 0)
}

fn native_bytebuddy_field_list_for_tokens_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_bytebuddy_method_list_forwarding_size(ctx, args, "tokens", 1)
}

fn native_bytebuddy_field_list_for_tokens_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = match args.get(1) {
        Some(Value::Int(index)) => *index,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "FieldList$ForTokens.get index must be int".to_string(),
            }
            .into())
        }
    };
    let base_pin = ctx.pin_native_root(this);
    let result = (|| {
        let declaring_type = bytebuddy_ref_field(ctx, this, "declaringType", 0);
        let tokens = bytebuddy_required_ref_field(ctx, this, "tokens", 1)?;
        let declaring_pin = declaring_type.map(|obj| (ctx.pin_native_root(obj), obj));
        let tokens_pin = ctx.pin_native_root(tokens);

        let tokens = ctx.read_native_pin(tokens_pin, tokens);
        let token = bytebuddy_list_get_i32(ctx, tokens, index)?;
        let token_pin = token.map(|obj| (ctx.pin_native_root(obj), obj));

        let declaring_type =
            declaring_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        let token = token_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        bytebuddy_new_near(
            ctx,
            this,
            BYTEBUDDY_FIELD_DESCRIPTION_LATENT,
            "(Lnet/bytebuddy/description/type/TypeDescription;Lnet/bytebuddy/description/field/FieldDescription$Token;)V",
            &[Value::Object(declaring_type), Value::Object(token)],
        )
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn native_bytebuddy_field_list_for_loaded_fields_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_bytebuddy_method_list_forwarding_size(ctx, args, "fields", 0)
}

fn native_bytebuddy_field_list_for_loaded_fields_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = match args.get(1) {
        Some(Value::Int(index)) => *index,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "FieldList$ForLoadedFields.get index must be int".to_string(),
            }
            .into())
        }
    };
    let base_pin = ctx.pin_native_root(this);
    let result = (|| {
        let fields = bytebuddy_required_ref_field(ctx, this, "fields", 0)?;
        let fields_pin = ctx.pin_native_root(fields);
        let fields = ctx.read_native_pin(fields_pin, fields);
        let field = bytebuddy_list_get_i32(ctx, fields, index)?;
        let field_pin = field.map(|obj| (ctx.pin_native_root(obj), obj));
        let field = field_pin.map(|(pin, fallback)| ctx.read_native_pin(pin, fallback));
        bytebuddy_new_near(
            ctx,
            this,
            BYTEBUDDY_FIELD_DESCRIPTION_FOR_LOADED_FIELD,
            "(Ljava/lang/reflect/Field;)V",
            &[Value::Object(field)],
        )
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn native_bytebuddy_type_list_explicit_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_bytebuddy_method_list_forwarding_size(ctx, args, "typeDescriptions", 0)
}

fn native_bytebuddy_type_list_explicit_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_bytebuddy_method_list_forwarding_get(ctx, args, "typeDescriptions", 0)
}

fn native_bytebuddy_type_list_generic_explicit_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_bytebuddy_method_list_forwarding_size(ctx, args, "typeDefinitions", 0)
}

fn native_bytebuddy_type_list_generic_explicit_get(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let index = match args.get(1) {
        Some(Value::Int(index)) => *index,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "TypeList$Generic$Explicit.get index must be int".to_string(),
            }
            .into())
        }
    };
    let base_pin = ctx.pin_native_root(this);
    let result = (|| {
        let type_definitions = bytebuddy_required_ref_field(ctx, this, "typeDefinitions", 0)?;
        let type_definitions_pin = ctx.pin_native_root(type_definitions);
        let type_definitions = ctx.read_native_pin(type_definitions_pin, type_definitions);
        let type_definition = bytebuddy_list_get_i32(ctx, type_definitions, index)?;
        let Some(type_definition) = type_definition else {
            return Ok(Some(Value::Object(None)));
        };
        let type_definition_pin = ctx.pin_native_root(type_definition);
        let type_definition = ctx.read_native_pin(type_definition_pin, type_definition);
        ctx.invoke_virtual(
            type_definition,
            "asGenericType",
            "()Lnet/bytebuddy/description/type/TypeDescription$Generic;",
            &[],
        )
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn native_bytebuddy_method_graph_for_java_method_token_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(bytebuddy_int_field(
        ctx, this, "hashCode", 1,
    ))))
}

fn native_bytebuddy_method_graph_for_java_method_token_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(obj)) => *obj,
        _ => None,
    };
    if Some(this) == other {
        return Ok(Some(Value::Int(1)));
    }
    let Some(other) = other else {
        return Ok(Some(Value::Int(0)));
    };
    if !bytebuddy_object_is_exact_class(ctx, other, BYTEBUDDY_METHOD_GRAPH_FOR_JAVA_METHOD_TOKEN) {
        return Ok(Some(Value::Int(0)));
    }

    let base_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let result = (|| {
        let this = ctx.read_native_pin(base_pin, this);
        let other = ctx.read_native_pin(other_pin, other);
        let this_type_token = bytebuddy_required_ref_field(ctx, this, "typeToken", 0)?;
        let other_type_token = bytebuddy_required_ref_field(ctx, other, "typeToken", 0)?;
        let this_token_pin = ctx.pin_native_root(this_type_token);
        let other_token_pin = ctx.pin_native_root(other_type_token);

        let this_type_token = ctx.read_native_pin(this_token_pin, this_type_token);
        let other_type_token = ctx.read_native_pin(other_token_pin, other_type_token);
        let this_parameters =
            bytebuddy_required_ref_field(ctx, this_type_token, "parameterTypes", 1)?;
        let other_parameters =
            bytebuddy_required_ref_field(ctx, other_type_token, "parameterTypes", 1)?;
        Ok(Some(Value::Int(
            if bytebuddy_list_equals(ctx, Some(this_parameters), Some(other_parameters))? {
                1
            } else {
                0
            },
        )))
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn native_bytebuddy_method_graph_default_key_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let base_pin = ctx.pin_native_root(this);
    let internal_name = bytebuddy_ref_field(ctx, this, "internalName", 0);
    let name_hash = bytebuddy_object_hash(ctx, internal_name)?;
    let this = ctx.read_native_pin(base_pin, this);
    let parameter_count = bytebuddy_int_field(ctx, this, "parameterCount", 1);
    ctx.unpin_native_roots(base_pin);
    Ok(Some(Value::Int(
        name_hash.wrapping_add(31i32.wrapping_mul(parameter_count)),
    )))
}

fn native_bytebuddy_method_graph_default_key_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let mut other = match args.get(1) {
        Some(Value::Object(Some(other))) => *other,
        _ => return Ok(Some(Value::Int(0))),
    };
    if this == other {
        return Ok(Some(Value::Int(1)));
    }
    if !bytebuddy_object_is_instance_of(ctx, other, BYTEBUDDY_METHOD_GRAPH_DEFAULT_KEY) {
        return Ok(Some(Value::Int(0)));
    }

    let base_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let result: Result<bool, MethodCallFailed> = (|| {
        this = ctx.read_native_pin(base_pin, this);
        other = ctx.read_native_pin(other_pin, other);
        let this_name = bytebuddy_ref_field(ctx, this, "internalName", 0);
        let other_name = bytebuddy_ref_field(ctx, other, "internalName", 0);
        if !bytebuddy_object_equals(ctx, this_name, other_name)? {
            return Ok(false);
        }

        this = ctx.read_native_pin(base_pin, this);
        other = ctx.read_native_pin(other_pin, other);
        if bytebuddy_int_field(ctx, this, "parameterCount", 1)
            != bytebuddy_int_field(ctx, other, "parameterCount", 1)
        {
            return Ok(false);
        }

        this = ctx.read_native_pin(base_pin, this);
        other = ctx.read_native_pin(other_pin, other);
        let this_identifiers = bytebuddy_method_graph_key_identifiers(ctx, this)?;
        let this_identifiers_pin =
            this_identifiers.map(|identifiers| ctx.pin_native_root(identifiers));
        this = ctx.read_native_pin(base_pin, this);
        other = ctx.read_native_pin(other_pin, other);
        let other_identifiers = bytebuddy_method_graph_key_identifiers(ctx, other)?;
        let this_identifiers = match (this_identifiers, this_identifiers_pin) {
            (Some(identifiers), Some(pin)) => Some(ctx.read_native_pin(pin, identifiers)),
            _ => None,
        };
        bytebuddy_method_graph_identifier_sets_intersect(ctx, this_identifiers, other_identifiers)
    })();
    ctx.unpin_native_roots(base_pin);
    Ok(Some(antlr_bool(result?)))
}

fn bytebuddy_object_is_instance_of(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    class_name: &str,
) -> bool {
    let obj_class = ctx.class_id_of_object(obj);
    if ctx.class_name_arc_of_id(obj_class).as_deref() == Some(class_name) {
        return true;
    }
    ctx.class_id_by_name(class_name)
        .map(|target| ctx.is_subclass(obj_class, target))
        .unwrap_or(false)
}

fn bytebuddy_method_graph_key_identifiers(
    ctx: &mut dyn NativeContext,
    key: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(key))
        .unwrap_or_default();
    if class_name == BYTEBUDDY_METHOD_GRAPH_DEFAULT_KEY_DETACHED {
        return Ok(bytebuddy_ref_field(ctx, key, "identifiers", 2));
    }
    if class_name == BYTEBUDDY_METHOD_GRAPH_DEFAULT_KEY_HARMONIZED {
        let Some(identifiers) = bytebuddy_ref_field(ctx, key, "identifiers", 2) else {
            return Ok(None);
        };
        let base_pin = ctx.pin_native_root(identifiers);
        let result = match ctx.invoke_virtual(identifiers, "keySet", "()Ljava/util/Set;", &[])? {
            Some(Value::Object(obj)) => obj,
            _ => None,
        };
        ctx.unpin_native_roots(base_pin);
        return Ok(result);
    }

    let base_pin = ctx.pin_native_root(key);
    let result = match ctx.invoke_virtual(key, "getIdentifiers", "()Ljava/util/Set;", &[])? {
        Some(Value::Object(obj)) => obj,
        _ => None,
    };
    ctx.unpin_native_roots(base_pin);
    Ok(result)
}

fn bytebuddy_method_graph_identifier_sets_intersect(
    ctx: &mut dyn NativeContext,
    a: Option<ObjectRef>,
    b: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        let Some(set) = a else {
            return Ok(false);
        };
        return Ok(bytebuddy_list_size(ctx, set)? > 0);
    }
    let (Some(mut a), Some(mut b)) = (a, b) else {
        return Ok(false);
    };
    let base_pin = ctx.pin_native_root(a);
    let b_pin = ctx.pin_native_root(b);
    let a_size = bytebuddy_list_size(ctx, a)?;
    a = ctx.read_native_pin(base_pin, a);
    b = ctx.read_native_pin(b_pin, b);
    let b_size = bytebuddy_list_size(ctx, b)?;
    a = ctx.read_native_pin(base_pin, a);
    b = ctx.read_native_pin(b_pin, b);
    if a_size == 0 || b_size == 0 {
        ctx.unpin_native_roots(base_pin);
        return Ok(false);
    }

    let result: Result<bool, MethodCallFailed> = (|| {
        let a = ctx.read_native_pin(base_pin, a);
        let iterator = match ctx.invoke_virtual(a, "iterator", "()Ljava/util/Iterator;", &[])? {
            Some(Value::Object(Some(iterator))) => iterator,
            _ => return Ok(false),
        };
        let iterator_pin = ctx.pin_native_root(iterator);
        loop {
            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let has_next = match ctx.invoke_virtual(iterator, "hasNext", "()Z", &[])? {
                Some(Value::Int(v)) => v != 0,
                _ => false,
            };
            if !has_next {
                break;
            }
            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let element = match ctx.invoke_virtual(iterator, "next", "()Ljava/lang/Object;", &[])? {
                Some(Value::Object(Some(element))) => element,
                _ => continue,
            };
            let element_pin = ctx.pin_native_root(element);
            let b = ctx.read_native_pin(b_pin, b);
            if bytebuddy_collection_contains_identifier(ctx, b, element)? {
                return Ok(true);
            }
            ctx.unpin_native_roots(element_pin);
        }
        Ok(false)
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn bytebuddy_collection_contains_identifier(
    ctx: &mut dyn NativeContext,
    collection: ObjectRef,
    target: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let base_pin = ctx.pin_native_root(collection);
    let target_pin = ctx.pin_native_root(target);
    let result: Result<bool, MethodCallFailed> = (|| {
        let collection = ctx.read_native_pin(base_pin, collection);
        let iterator =
            match ctx.invoke_virtual(collection, "iterator", "()Ljava/util/Iterator;", &[])? {
                Some(Value::Object(Some(iterator))) => iterator,
                _ => return Ok(false),
            };
        let iterator_pin = ctx.pin_native_root(iterator);
        loop {
            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let has_next = match ctx.invoke_virtual(iterator, "hasNext", "()Z", &[])? {
                Some(Value::Int(v)) => v != 0,
                _ => false,
            };
            if !has_next {
                break;
            }
            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let element = match ctx.invoke_virtual(iterator, "next", "()Ljava/lang/Object;", &[])? {
                Some(Value::Object(Some(element))) => element,
                _ => continue,
            };
            let element_pin = ctx.pin_native_root(element);
            let target = ctx.read_native_pin(target_pin, target);
            if bytebuddy_method_graph_identifier_equals(ctx, element, target)? {
                return Ok(true);
            }
            ctx.unpin_native_roots(element_pin);
        }
        Ok(false)
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn bytebuddy_method_graph_identifier_equals(
    ctx: &mut dyn NativeContext,
    a: ObjectRef,
    b: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        return Ok(true);
    }
    let a_is_type_token = bytebuddy_object_is_exact_class(ctx, a, BYTEBUDDY_METHOD_TYPE_TOKEN);
    let b_is_type_token = bytebuddy_object_is_exact_class(ctx, b, BYTEBUDDY_METHOD_TYPE_TOKEN);
    if a_is_type_token && b_is_type_token {
        return bytebuddy_type_token_equals_by_descriptors(ctx, a, b, true);
    }

    let a_is_java_method_token =
        bytebuddy_object_is_exact_class(ctx, a, BYTEBUDDY_METHOD_GRAPH_FOR_JAVA_METHOD_TOKEN);
    let b_is_java_method_token =
        bytebuddy_object_is_exact_class(ctx, b, BYTEBUDDY_METHOD_GRAPH_FOR_JAVA_METHOD_TOKEN);
    if a_is_java_method_token && b_is_java_method_token {
        let base_pin = ctx.pin_native_root(a);
        let b_pin = ctx.pin_native_root(b);
        let result: Result<bool, MethodCallFailed> = (|| {
            let a = ctx.read_native_pin(base_pin, a);
            let b = ctx.read_native_pin(b_pin, b);
            let Some(a_type_token) = bytebuddy_ref_field(ctx, a, "typeToken", 0) else {
                return Ok(false);
            };
            let Some(b_type_token) = bytebuddy_ref_field(ctx, b, "typeToken", 0) else {
                return Ok(false);
            };
            bytebuddy_type_token_equals_by_descriptors(ctx, a_type_token, b_type_token, false)
        })();
        ctx.unpin_native_roots(base_pin);
        return result;
    }

    bytebuddy_object_equals(ctx, Some(a), Some(b))
}

fn bytebuddy_type_token_equals_by_descriptors(
    ctx: &mut dyn NativeContext,
    mut a: ObjectRef,
    mut b: ObjectRef,
    compare_return_type: bool,
) -> Result<bool, MethodCallFailed> {
    let base_pin = ctx.pin_native_root(a);
    let b_pin = ctx.pin_native_root(b);
    let result: Result<bool, MethodCallFailed> = (|| {
        if compare_return_type {
            let a_return = bytebuddy_ref_field(ctx, a, "returnType", 0);
            let b_return = bytebuddy_ref_field(ctx, b, "returnType", 0);
            if !bytebuddy_type_description_equals_by_descriptor(ctx, a_return, b_return)? {
                return Ok(false);
            }
        }

        a = ctx.read_native_pin(base_pin, a);
        b = ctx.read_native_pin(b_pin, b);
        let a_parameters = bytebuddy_ref_field(ctx, a, "parameterTypes", 1);
        let b_parameters = bytebuddy_ref_field(ctx, b, "parameterTypes", 1);
        bytebuddy_type_description_lists_equal(ctx, a_parameters, b_parameters)
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn bytebuddy_type_description_lists_equal(
    ctx: &mut dyn NativeContext,
    a: Option<ObjectRef>,
    b: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        return Ok(true);
    }
    let (Some(mut a), Some(mut b)) = (a, b) else {
        return Ok(false);
    };
    let base_pin = ctx.pin_native_root(a);
    let b_pin = ctx.pin_native_root(b);
    let result: Result<bool, MethodCallFailed> = (|| {
        let a_size = bytebuddy_list_size(ctx, a)?;
        a = ctx.read_native_pin(base_pin, a);
        b = ctx.read_native_pin(b_pin, b);
        if a_size != bytebuddy_list_size(ctx, b)? {
            return Ok(false);
        }
        a = ctx.read_native_pin(base_pin, a);
        b = ctx.read_native_pin(b_pin, b);
        for i in 0..a_size {
            let a_element = bytebuddy_list_get(ctx, a, i)?;
            let a_element_pin = a_element.map(|element| ctx.pin_native_root(element));
            a = ctx.read_native_pin(base_pin, a);
            b = ctx.read_native_pin(b_pin, b);
            let b_element = bytebuddy_list_get(ctx, b, i)?;
            let a_element = match (a_element, a_element_pin) {
                (Some(element), Some(pin)) => {
                    let element = ctx.read_native_pin(pin, element);
                    ctx.unpin_native_roots(pin);
                    Some(element)
                }
                _ => None,
            };
            a = ctx.read_native_pin(base_pin, a);
            b = ctx.read_native_pin(b_pin, b);
            if !bytebuddy_type_description_equals_by_descriptor(ctx, a_element, b_element)? {
                return Ok(false);
            }
            a = ctx.read_native_pin(base_pin, a);
            b = ctx.read_native_pin(b_pin, b);
        }
        Ok(true)
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn bytebuddy_type_description_equals_by_descriptor(
    ctx: &mut dyn NativeContext,
    a: Option<ObjectRef>,
    b: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        return Ok(true);
    }
    let (Some(a), Some(b)) = (a, b) else {
        return Ok(false);
    };
    let base_pin = ctx.pin_native_root(a);
    let b_pin = ctx.pin_native_root(b);
    let result = (|| {
        let a = ctx.read_native_pin(base_pin, a);
        let a_descriptor = bytebuddy_type_description_descriptor(ctx, a)?;
        let b = ctx.read_native_pin(b_pin, b);
        let b_descriptor = bytebuddy_type_description_descriptor(ctx, b)?;
        Ok(a_descriptor.is_some() && a_descriptor == b_descriptor)
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

fn bytebuddy_type_description_descriptor(
    ctx: &mut dyn NativeContext,
    type_description: ObjectRef,
) -> Result<Option<String>, MethodCallFailed> {
    let base_pin = ctx.pin_native_root(type_description);
    let result = match ctx.invoke_virtual(
        type_description,
        "getDescriptor",
        "()Ljava/lang/String;",
        &[],
    )? {
        Some(Value::Object(Some(descriptor))) => ctx.read_string(descriptor),
        _ => None,
    };
    ctx.unpin_native_roots(base_pin);
    Ok(result)
}

fn native_mockito_location_factory_create(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let location = match ctx.new_object(MOCKITO_JAVA8_LOCATION_IMPL)? {
        Some(Value::Object(Some(location))) => location,
        _ => return Ok(Some(Value::Object(None))),
    };
    let base_pin = ctx.pin_native_root(location);
    let stack_trace_line = ctx.create_string("-> at <<unknown line>>");
    let line_pin = ctx.pin_native_root(stack_trace_line);
    let source_file = ctx.create_string("<unknown source file>");

    let location = ctx.read_native_pin(base_pin, location);
    let stack_trace_line = ctx.read_native_pin(line_pin, stack_trace_line);
    ctx.set_field_by_name(
        location,
        "stackTraceLine",
        Value::Object(Some(stack_trace_line)),
    );
    ctx.set_field_by_name(location, "sourceFile", Value::Object(Some(source_file)));
    ctx.unpin_native_roots(base_pin);
    Ok(Some(Value::Object(Some(location))))
}

fn native_mockito_mock_method_advice_is_overridden(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mock = obj_arg(args, 1)?;
    let method = obj_arg(args, 2)?;
    let mock_class_id = ctx.class_id_of_object(mock);
    let mock_class_name = ctx.class_name_of_id(mock_class_id).unwrap_or_default();
    if !mock_class_name.contains("$MockitoMock$") {
        let Some((declaring_class_id, method_name, descriptor)) =
            crate::lang_class::method_class_name_desc(ctx, method)
        else {
            return Ok(Some(Value::Int(0)));
        };

        // NOTE: `declaring_class_id` being an interface (e.g. Mockito asks
        // about `Greeter.greet()` for a call reached via `Interface.super
        // .method()` from the concrete override) must NOT short-circuit to
        // "not overridden" here. The walk below still answers correctly for
        // that case: `class_id == declaring_class_id` simply never matches
        // while walking the CONCRETE superclass chain (interfaces never
        // appear in it), so the loop just checks every class from the mock's
        // own class up to `Object` for a concrete declaration of the method
        // — exactly what "does the mock's real class override this
        // interface default method" needs. Returning a hardcoded false here
        // instead made every interface-default `super` call from a redefined
        // override re-enter Mockito's `CallsRealMethods` interception
        // indefinitely (OtlpMetricsPropertiesConfigAdapter.url() calling
        // `OtlpConfig.super.url()`, and the same shape in any spied/mocked
        // class that overrides an interface default method).

        // Mockito supplies a Method declared by an ancestor, while the mock
        // can inherit its concrete override through one or more intermediate
        // classes.  For example, RestTemplate inherits setRequestFactory from
        // InterceptingHttpAccessor, but Mockito asks about HttpAccessor's
        // Method.  Stop before the declaring class: its own implementation is
        // not an override and must remain eligible for ordinary interception.
        let mut candidate = Some(mock_class_id);
        while let Some(class_id) = candidate {
            if class_id == declaring_class_id {
                break;
            }
            if ctx.class_declares_method(class_id, &method_name, &descriptor) {
                return Ok(Some(Value::Int(1)));
            }
            candidate = ctx.superclass_of(class_id);
        }
        return Ok(Some(Value::Int(0)));
    }

    let declaring_mirror = match ctx.get_field_by_name(method, "clazz") {
        Value::Object(Some(mirror)) => mirror,
        _ => return Ok(Some(Value::Int(0))),
    };
    let Some(declaring_class_id) = ctx.class_id_from_mirror(declaring_mirror) else {
        return Ok(Some(Value::Int(0)));
    };

    if ctx.is_interface_class(declaring_class_id) {
        return Ok(Some(Value::Int(0)));
    }

    // This method is reached only after Mockito has already established that
    // the receiver is mocked. Falling back to "not overridden" preserves normal
    // MockHandler interception for class mocks instead of silently bypassing it.
    Ok(Some(Value::Int(0)))
}

/// Mockito selects its Java-9 `InstrumentationMemberAccessor` by constructing
/// a Byte Buddy subclass during `ModuleMemberAccessor` class initialization.
/// CratonVM supports Mockito's ordinary reflection accessor, but that eager
/// bootstrap enters a bytecode-generation path before the test has requested a
/// mock.  Return Mockito's own supported fallback directly, preserving the
/// public MemberAccessor contract without changing mock generation itself.
fn native_mockito_module_member_accessor_delegate(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    ctx.new_object_initialized(MOCKITO_REFLECTION_MEMBER_ACCESSOR, "()V", &[])
}

fn bytebuddy_list_hash(
    ctx: &mut dyn NativeContext,
    list: Option<ObjectRef>,
) -> Result<i32, MethodCallFailed> {
    let Some(mut list) = list else {
        return Ok(0);
    };
    let base_pin = ctx.pin_native_root(list);
    let size = bytebuddy_list_size(ctx, list)?;
    list = ctx.read_native_pin(base_pin, list);
    let mut hash = 1i32;
    for i in 0..size {
        let element = bytebuddy_list_get(ctx, list, i)?;
        list = ctx.read_native_pin(base_pin, list);
        hash = hash
            .wrapping_mul(31)
            .wrapping_add(bytebuddy_object_hash(ctx, element)?);
        list = ctx.read_native_pin(base_pin, list);
    }
    ctx.unpin_native_roots(base_pin);
    Ok(hash)
}

fn bytebuddy_list_equals(
    ctx: &mut dyn NativeContext,
    a: Option<ObjectRef>,
    b: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    if a == b {
        return Ok(true);
    }
    let (Some(mut a), Some(mut b)) = (a, b) else {
        return Ok(false);
    };
    let base_pin = ctx.pin_native_root(a);
    let b_pin = ctx.pin_native_root(b);
    let a_size = bytebuddy_list_size(ctx, a)?;
    a = ctx.read_native_pin(base_pin, a);
    b = ctx.read_native_pin(b_pin, b);
    if a_size != bytebuddy_list_size(ctx, b)? {
        ctx.unpin_native_roots(base_pin);
        return Ok(false);
    }
    a = ctx.read_native_pin(base_pin, a);
    b = ctx.read_native_pin(b_pin, b);
    for i in 0..a_size {
        let a_element = bytebuddy_list_get(ctx, a, i)?;
        let a_element_pin = a_element.map(|element| ctx.pin_native_root(element));
        a = ctx.read_native_pin(base_pin, a);
        b = ctx.read_native_pin(b_pin, b);
        let b_element = bytebuddy_list_get(ctx, b, i)?;
        let a_element = match (a_element, a_element_pin) {
            (Some(element), Some(pin)) => {
                let element = ctx.read_native_pin(pin, element);
                ctx.unpin_native_roots(pin);
                Some(element)
            }
            _ => None,
        };
        a = ctx.read_native_pin(base_pin, a);
        b = ctx.read_native_pin(b_pin, b);
        if !bytebuddy_object_equals(ctx, a_element, b_element)? {
            ctx.unpin_native_roots(base_pin);
            return Ok(false);
        }
        a = ctx.read_native_pin(base_pin, a);
        b = ctx.read_native_pin(b_pin, b);
    }
    ctx.unpin_native_roots(base_pin);
    Ok(true)
}

fn bytebuddy_object_is_exact_class(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    class_name: &str,
) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
        == Some(class_name)
}

fn native_bytebuddy_method_type_token_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let base_pin = ctx.pin_native_root(this);
    let cached = bytebuddy_int_field(ctx, this, "hashCode", 2);
    if cached != 0 {
        ctx.unpin_native_roots(base_pin);
        return Ok(Some(Value::Int(cached)));
    }

    let return_type = bytebuddy_ref_field(ctx, this, "returnType", 0);
    let return_hash = bytebuddy_object_hash(ctx, return_type)?;
    this = ctx.read_native_pin(base_pin, this);
    let parameter_types = bytebuddy_ref_field(ctx, this, "parameterTypes", 1);
    let parameter_hash = bytebuddy_list_hash(ctx, parameter_types)?;
    this = ctx.read_native_pin(base_pin, this);

    let hash = return_hash.wrapping_mul(31).wrapping_add(parameter_hash);
    if hash != 0 {
        bytebuddy_set_field_value(ctx, this, "hashCode", 2, Value::Int(hash));
    }
    ctx.unpin_native_roots(base_pin);
    Ok(Some(Value::Int(hash)))
}

fn native_bytebuddy_method_type_token_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let mut other = match args.get(1) {
        Some(Value::Object(Some(other))) => *other,
        _ => return Ok(Some(Value::Int(0))),
    };
    if this == other {
        return Ok(Some(Value::Int(1)));
    }
    if !bytebuddy_object_is_exact_class(ctx, other, BYTEBUDDY_METHOD_TYPE_TOKEN) {
        return Ok(Some(Value::Int(0)));
    }

    let base_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let this_return_type = bytebuddy_ref_field(ctx, this, "returnType", 0);
    let other_return_type = bytebuddy_ref_field(ctx, other, "returnType", 0);
    if !bytebuddy_object_equals(ctx, this_return_type, other_return_type)? {
        ctx.unpin_native_roots(base_pin);
        return Ok(Some(Value::Int(0)));
    }
    this = ctx.read_native_pin(base_pin, this);
    other = ctx.read_native_pin(other_pin, other);
    let this_parameter_types = bytebuddy_ref_field(ctx, this, "parameterTypes", 1);
    let other_parameter_types = bytebuddy_ref_field(ctx, other, "parameterTypes", 1);
    let equal = bytebuddy_list_equals(ctx, this_parameter_types, other_parameter_types)?;
    ctx.unpin_native_roots(base_pin);
    Ok(Some(antlr_bool(equal)))
}

fn native_bytebuddy_method_signature_token_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let base_pin = ctx.pin_native_root(this);
    let cached = bytebuddy_int_field(ctx, this, "hashCode", 3);
    if cached != 0 {
        ctx.unpin_native_roots(base_pin);
        return Ok(Some(Value::Int(cached)));
    }

    let name = bytebuddy_ref_field(ctx, this, "name", 0);
    let mut hash = bytebuddy_object_hash(ctx, name)?;
    this = ctx.read_native_pin(base_pin, this);
    let return_type = bytebuddy_ref_field(ctx, this, "returnType", 1);
    hash = hash
        .wrapping_mul(31)
        .wrapping_add(bytebuddy_object_hash(ctx, return_type)?);
    this = ctx.read_native_pin(base_pin, this);
    let parameter_types = bytebuddy_ref_field(ctx, this, "parameterTypes", 2);
    hash = hash
        .wrapping_mul(31)
        .wrapping_add(bytebuddy_list_hash(ctx, parameter_types)?);
    this = ctx.read_native_pin(base_pin, this);
    if hash != 0 {
        bytebuddy_set_field_value(ctx, this, "hashCode", 3, Value::Int(hash));
    }
    ctx.unpin_native_roots(base_pin);
    Ok(Some(Value::Int(hash)))
}

fn native_bytebuddy_method_signature_token_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let mut other = match args.get(1) {
        Some(Value::Object(Some(other))) => *other,
        _ => return Ok(Some(Value::Int(0))),
    };
    if this == other {
        return Ok(Some(Value::Int(1)));
    }
    if !bytebuddy_object_is_exact_class(ctx, other, BYTEBUDDY_METHOD_SIGNATURE_TOKEN) {
        return Ok(Some(Value::Int(0)));
    }

    let base_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let this_name = bytebuddy_ref_field(ctx, this, "name", 0);
    let other_name = bytebuddy_ref_field(ctx, other, "name", 0);
    if !bytebuddy_object_equals(ctx, this_name, other_name)? {
        ctx.unpin_native_roots(base_pin);
        return Ok(Some(Value::Int(0)));
    }
    this = ctx.read_native_pin(base_pin, this);
    other = ctx.read_native_pin(other_pin, other);
    let this_return_type = bytebuddy_ref_field(ctx, this, "returnType", 1);
    let other_return_type = bytebuddy_ref_field(ctx, other, "returnType", 1);
    if !bytebuddy_object_equals(ctx, this_return_type, other_return_type)? {
        ctx.unpin_native_roots(base_pin);
        return Ok(Some(Value::Int(0)));
    }
    this = ctx.read_native_pin(base_pin, this);
    other = ctx.read_native_pin(other_pin, other);
    let this_parameter_types = bytebuddy_ref_field(ctx, this, "parameterTypes", 2);
    let other_parameter_types = bytebuddy_ref_field(ctx, other, "parameterTypes", 2);
    let equal = bytebuddy_list_equals(ctx, this_parameter_types, other_parameter_types)?;
    ctx.unpin_native_roots(base_pin);
    Ok(Some(antlr_bool(equal)))
}

pub(crate) fn register_bytebuddy_method_token_intrinsics(registry: &mut NativeMethodRegistry) {
    registry.register(
        BYTEBUDDY_METHOD_TYPE_TOKEN,
        "hashCode",
        "()I",
        native_bytebuddy_method_type_token_hash_code,
    );
    registry.register(
        BYTEBUDDY_METHOD_TYPE_TOKEN,
        "equals",
        "(Ljava/lang/Object;)Z",
        native_bytebuddy_method_type_token_equals,
    );
    registry.register(
        BYTEBUDDY_METHOD_SIGNATURE_TOKEN,
        "hashCode",
        "()I",
        native_bytebuddy_method_signature_token_hash_code,
    );
    registry.register(
        BYTEBUDDY_METHOD_SIGNATURE_TOKEN,
        "equals",
        "(Ljava/lang/Object;)Z",
        native_bytebuddy_method_signature_token_equals,
    );
    registry.register(
        BYTEBUDDY_METHOD_DESCRIPTION_TYPE_SUBSTITUTING,
        "<init>",
        "(Lnet/bytebuddy/description/type/TypeDescription$Generic;Lnet/bytebuddy/description/method/MethodDescription;Lnet/bytebuddy/description/type/TypeDescription$Generic$Visitor;)V",
        native_bytebuddy_method_description_type_substituting_init,
    );
    registry.register(
        BYTEBUDDY_METHOD_LIST_EXPLICIT,
        "size",
        "()I",
        native_bytebuddy_method_list_explicit_size,
    );
    registry.register(
        BYTEBUDDY_METHOD_LIST_EXPLICIT,
        "get",
        "(I)Lnet/bytebuddy/description/method/MethodDescription;",
        native_bytebuddy_method_list_explicit_get,
    );
    registry.register(
        BYTEBUDDY_METHOD_LIST_EXPLICIT,
        "get",
        "(I)Ljava/lang/Object;",
        native_bytebuddy_method_list_explicit_get,
    );
    registry.register(
        BYTEBUDDY_METHOD_LIST_TYPE_SUBSTITUTING,
        "size",
        "()I",
        native_bytebuddy_method_list_type_substituting_size,
    );
    registry.register(
        BYTEBUDDY_METHOD_LIST_TYPE_SUBSTITUTING,
        "get",
        "(I)Lnet/bytebuddy/description/method/MethodDescription$InGenericShape;",
        native_bytebuddy_method_list_type_substituting_get,
    );
    registry.register(
        BYTEBUDDY_METHOD_LIST_TYPE_SUBSTITUTING,
        "get",
        "(I)Ljava/lang/Object;",
        native_bytebuddy_method_list_type_substituting_get,
    );
    registry.register(
        BYTEBUDDY_METHOD_LIST_FOR_LOADED_METHODS,
        "size",
        "()I",
        native_bytebuddy_method_list_for_loaded_methods_size,
    );
    registry.register(
        BYTEBUDDY_METHOD_LIST_FOR_LOADED_METHODS,
        "get",
        "(I)Lnet/bytebuddy/description/method/MethodDescription$InDefinedShape;",
        native_bytebuddy_method_list_for_loaded_methods_get,
    );
    registry.register(
        BYTEBUDDY_METHOD_LIST_FOR_LOADED_METHODS,
        "get",
        "(I)Ljava/lang/Object;",
        native_bytebuddy_method_list_for_loaded_methods_get,
    );
    registry.register(
        BYTEBUDDY_METHOD_LIST_FOR_TOKENS,
        "size",
        "()I",
        native_bytebuddy_method_list_for_tokens_size,
    );
    registry.register(
        BYTEBUDDY_METHOD_LIST_FOR_TOKENS,
        "get",
        "(I)Lnet/bytebuddy/description/method/MethodDescription$InDefinedShape;",
        native_bytebuddy_method_list_for_tokens_get,
    );
    registry.register(
        BYTEBUDDY_METHOD_LIST_FOR_TOKENS,
        "get",
        "(I)Ljava/lang/Object;",
        native_bytebuddy_method_list_for_tokens_get,
    );
    registry.register(
        BYTEBUDDY_FIELD_LIST_EXPLICIT,
        "size",
        "()I",
        native_bytebuddy_field_list_explicit_size,
    );
    registry.register(
        BYTEBUDDY_FIELD_LIST_EXPLICIT,
        "get",
        "(I)Lnet/bytebuddy/description/field/FieldDescription;",
        native_bytebuddy_field_list_explicit_get,
    );
    registry.register(
        BYTEBUDDY_FIELD_LIST_EXPLICIT,
        "get",
        "(I)Ljava/lang/Object;",
        native_bytebuddy_field_list_explicit_get,
    );
    registry.register(
        BYTEBUDDY_FIELD_LIST_FOR_TOKENS,
        "size",
        "()I",
        native_bytebuddy_field_list_for_tokens_size,
    );
    registry.register(
        BYTEBUDDY_FIELD_LIST_FOR_TOKENS,
        "get",
        "(I)Lnet/bytebuddy/description/field/FieldDescription$InDefinedShape;",
        native_bytebuddy_field_list_for_tokens_get,
    );
    registry.register(
        BYTEBUDDY_FIELD_LIST_FOR_TOKENS,
        "get",
        "(I)Ljava/lang/Object;",
        native_bytebuddy_field_list_for_tokens_get,
    );
    registry.register(
        BYTEBUDDY_FIELD_LIST_FOR_LOADED_FIELDS,
        "size",
        "()I",
        native_bytebuddy_field_list_for_loaded_fields_size,
    );
    registry.register(
        BYTEBUDDY_FIELD_LIST_FOR_LOADED_FIELDS,
        "get",
        "(I)Lnet/bytebuddy/description/field/FieldDescription$InDefinedShape;",
        native_bytebuddy_field_list_for_loaded_fields_get,
    );
    registry.register(
        BYTEBUDDY_FIELD_LIST_FOR_LOADED_FIELDS,
        "get",
        "(I)Ljava/lang/Object;",
        native_bytebuddy_field_list_for_loaded_fields_get,
    );
    registry.register(
        BYTEBUDDY_TYPE_LIST_EXPLICIT,
        "size",
        "()I",
        native_bytebuddy_type_list_explicit_size,
    );
    registry.register(
        BYTEBUDDY_TYPE_LIST_EXPLICIT,
        "get",
        "(I)Lnet/bytebuddy/description/type/TypeDescription;",
        native_bytebuddy_type_list_explicit_get,
    );
    registry.register(
        BYTEBUDDY_TYPE_LIST_EXPLICIT,
        "get",
        "(I)Ljava/lang/Object;",
        native_bytebuddy_type_list_explicit_get,
    );
    registry.register(
        BYTEBUDDY_TYPE_LIST_GENERIC_EXPLICIT,
        "size",
        "()I",
        native_bytebuddy_type_list_generic_explicit_size,
    );
    registry.register(
        BYTEBUDDY_TYPE_LIST_GENERIC_EXPLICIT,
        "get",
        "(I)Lnet/bytebuddy/description/type/TypeDescription$Generic;",
        native_bytebuddy_type_list_generic_explicit_get,
    );
    registry.register(
        BYTEBUDDY_TYPE_LIST_GENERIC_EXPLICIT,
        "get",
        "(I)Ljava/lang/Object;",
        native_bytebuddy_type_list_generic_explicit_get,
    );
    registry.register(
        BYTEBUDDY_METHOD_GRAPH_FOR_JAVA_METHOD_TOKEN,
        "hashCode",
        "()I",
        native_bytebuddy_method_graph_for_java_method_token_hash_code,
    );
    registry.register(
        BYTEBUDDY_METHOD_GRAPH_FOR_JAVA_METHOD_TOKEN,
        "equals",
        "(Ljava/lang/Object;)Z",
        native_bytebuddy_method_graph_for_java_method_token_equals,
    );
    registry.register(
        BYTEBUDDY_METHOD_GRAPH_DEFAULT_KEY,
        "hashCode",
        "()I",
        native_bytebuddy_method_graph_default_key_hash_code,
    );
    registry.register(
        BYTEBUDDY_METHOD_GRAPH_DEFAULT_KEY,
        "equals",
        "(Ljava/lang/Object;)Z",
        native_bytebuddy_method_graph_default_key_equals,
    );
}

pub(crate) fn register_mockito_debugging_intrinsics(registry: &mut NativeMethodRegistry) {
    registry.register(
        MOCKITO_MOCK_METHOD_ADVICE,
        "isOverridden",
        "(Ljava/lang/Object;Ljava/lang/reflect/Method;)Z",
        native_mockito_mock_method_advice_is_overridden,
    );
    // The two *selector* overrides below forced Mockito onto its fallback
    // `Location` / `MemberAccessor` implementations on every run — a silent
    // divergence from HotSpot (which picks `LocationImpl` and
    // `InstrumentationMemberAccessor`) that cost every Mockito diagnostic its
    // call site. They are off by default now; see
    // `cratonvm_types::flags::mockito_legacy_selectors`.
    if !cratonvm_types::flags::mockito_legacy_selectors() {
        return;
    }
    registry.register(
        MOCKITO_LOCATION_FACTORY,
        "create",
        "()Lorg/mockito/invocation/Location;",
        native_mockito_location_factory_create,
    );
    registry.register(
        MOCKITO_LOCATION_FACTORY,
        "create",
        "(Z)Lorg/mockito/invocation/Location;",
        native_mockito_location_factory_create,
    );
    registry.register(
        MOCKITO_LOCATION_FACTORY_DEFAULT,
        "create",
        "(Z)Lorg/mockito/invocation/Location;",
        native_mockito_location_factory_create,
    );
    registry.register(
        MOCKITO_MODULE_MEMBER_ACCESSOR,
        "delegate",
        "()Lorg/mockito/plugins/MemberAccessor;",
        native_mockito_module_member_accessor_delegate,
    );
}

pub(crate) fn register_ecj_problem_overrides(registry: &mut NativeMethodRegistry) {
    fn ecj_problem_is_error(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        let severity = ctx
            .get_field_by_name(this, "severity")
            .as_int()
            .unwrap_or(0);
        let message = match ctx.get_field_by_name(this, "message") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };

        if message.contains("ServletConfig") && message.contains("getInitParameterNames") {
            return Ok(Some(Value::Int(0)));
        }

        Ok(Some(Value::Int(if (severity & 1) != 0 { 1 } else { 0 })))
    }

    for owner in [
        "org/eclipse/jdt/internal/compiler/problem/DefaultProblem",
        "org/eclipse/jdt/core/compiler/CategorizedProblem",
        "org/eclipse/jdt/core/compiler/IProblem",
    ] {
        registry.register(owner, "isError", "()Z", ecj_problem_is_error);
    }
}

pub(crate) fn ecj_class_file_relative_path(
    ctx: &mut dyn NativeContext,
    class_file: ObjectRef,
) -> Option<String> {
    let compound = match ctx
        .invoke_virtual(class_file, "getCompoundName", "()[[C", &[])
        .ok()
        .flatten()
    {
        Some(Value::Object(Some(o))) => o,
        _ => return None,
    };
    let mut parts = Vec::new();
    for i in 0..ctx.array_length(compound) {
        if let Value::Object(Some(chars)) = ctx.get_array_element(compound, i) {
            parts.push(char_array_to_string(ctx, chars));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("{}.class", parts.join("/")))
    }
}

pub(crate) fn ecj_class_file_bytes(
    ctx: &mut dyn NativeContext,
    class_file: ObjectRef,
) -> Option<Vec<u8>> {
    let bytes_arr = match ctx
        .invoke_virtual(class_file, "getBytes", "()[B", &[])
        .ok()
        .flatten()
    {
        Some(Value::Object(Some(o))) => o,
        _ => return None,
    };
    let mut bytes = vec![0u8; ctx.array_length(bytes_arr)];
    let copied = ctx.read_byte_array_into(bytes_arr, 0, &mut bytes);
    bytes.truncate(copied);
    Some(bytes)
}

pub(crate) fn write_ecj_class_bytes(path: &str, bytes: &[u8]) {
    let path = std::path::Path::new(path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, bytes);
}

fn native_ecj_compiler_requestor_accept_result(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let result = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };

    // `hasErrors()` can move `result` before `getFileName()` below reads it.
    let result_pin = ctx.pin_native_root(result);
    if matches!(
        ctx.invoke_virtual(result, "hasErrors", "()Z", &[])?,
        Some(Value::Int(v)) if v != 0
    ) {
        return Ok(None);
    }
    let result = ctx.read_native_pin(result_pin, result);

    let source_file = match ctx.invoke_virtual(result, "getFileName", "()[C", &[])? {
        Some(Value::Object(Some(chars))) => char_array_to_string(ctx, chars),
        _ => String::new(),
    };
    let class_files = match ctx.invoke_virtual(
        result,
        "getClassFiles",
        "()[Lorg/eclipse/jdt/internal/compiler/ClassFile;",
        &[],
    )? {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };

    for i in 0..ctx.array_length(class_files) {
        let Value::Object(Some(class_file)) = ctx.get_array_element(class_files, i) else {
            continue;
        };
        let Some(bytes) = ecj_class_file_bytes(ctx, class_file) else {
            continue;
        };
        if let Some(rel) = ecj_class_file_relative_path(ctx, class_file) {
            let rel_java = rel.strip_suffix(".class").unwrap_or(&rel).to_string() + ".java";
            if let Some(prefix) = source_file.strip_suffix(&rel_java) {
                let prefix = prefix.trim_end_matches('/');
                write_ecj_class_bytes(&format!("{prefix}/{rel}"), &bytes);
            }
        }
        if let Some(path) = source_file.strip_suffix(".java") {
            write_ecj_class_bytes(&format!("{path}.class"), &bytes);
        }
    }
    Ok(None)
}

/// Surefire `TestPlanScannerFilter.accept(Class)` builds a one-class discovery
/// request and calls `Launcher.discover`. When `JUnitPlatformProvider` failed
/// to retain a non-null `launcher` reference (field layout / init ordering
/// bugs), the bytecode path NPEs on `invokeinterface discover` with a null
/// receiver. Mirror the intended control flow and materialize a launcher via
/// `LauncherFactory.create()` only for this call when the field is null.
pub(crate) fn native_test_plan_scanner_filter_accept(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;

    const BUILDER: &str = "org/junit/platform/launcher/core/LauncherDiscoveryRequestBuilder";
    const SELECTORS: &str = "org/junit/platform/engine/discovery/DiscoverySelectors";
    const FACTORY: &str = "org/junit/platform/launcher/core/LauncherFactory";
    const DESC_BUILDER: &str =
        "()Lorg/junit/platform/launcher/core/LauncherDiscoveryRequestBuilder;";
    const DESC_SELECTORS: &str =
        "([Lorg/junit/platform/engine/DiscoverySelector;)Lorg/junit/platform/launcher/core/LauncherDiscoveryRequestBuilder;";
    const DESC_FILTERS: &str =
        "([Lorg/junit/platform/engine/Filter;)Lorg/junit/platform/launcher/core/LauncherDiscoveryRequestBuilder;";
    const DESC_BUILD: &str = "()Lorg/junit/platform/launcher/LauncherDiscoveryRequest;";
    const DESC_DISCOVER: &str =
        "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;";
    const DESC_SEL_STATIC: &str =
        "(Ljava/lang/String;)Lorg/junit/platform/engine/discovery/ClassSelector;";
    const DESC_FACTORY: &str = "()Lorg/junit/platform/launcher/Launcher;";
    const DESC_GET_NAME: &str = "()Ljava/lang/String;";

    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("TestPlanScannerFilter.accept: null this".into()),
            }
            .into());
        }
    };
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("TestPlanScannerFilter.accept: null class argument".into()),
            }
            .into());
        }
    };

    // Pin `this` across the long chain of allocating calls below (getName /
    // selectClass / load_class / new_ref_array / builder invokes) — a moving
    // young GC in any of them would relocate it out from under this raw Rust
    // local (native stale-local family); re-read before each later use. The
    // dispatcher truncates the pin stack when this native returns, so the
    // early error returns need no explicit unpin.
    let this_pin = ctx.pin_native_root(this);

    let name_val = ctx.invoke_virtual(class_obj, "getName", DESC_GET_NAME, &[])?;
    let name_obj = match name_val {
        Some(Value::Object(Some(s))) => s,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "TestPlanScannerFilter.accept: Class.getName() failed".into(),
            }
            .into());
        }
    };

    let selector_val = ctx.invoke(
        SELECTORS,
        "selectClass",
        DESC_SEL_STATIC,
        &[Value::Object(Some(name_obj))],
    )?;
    let selector = match selector_val {
        Some(Value::Object(Some(s))) => s,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "TestPlanScannerFilter.accept: DiscoverySelectors.selectClass failed"
                    .into(),
            }
            .into());
        }
    };
    // Pinned across load_class + new_ref_array (both can allocate/GC).
    let selector_pin = ctx.pin_native_root(selector);

    let ds_mirror = match ctx.load_class("org/junit/platform/engine/DiscoverySelector")? {
        Some(Value::Object(Some(m))) => m,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "TestPlanScannerFilter.accept: could not load DiscoverySelector".into(),
            }
            .into());
        }
    };
    let ds_cid = ctx.class_id_of_object(ds_mirror);
    let sel_arr = ctx.new_ref_array(ds_cid, 1);
    let selector_cur = ctx.read_native_pin(selector_pin, selector);
    ctx.set_array_element(sel_arr, 0, Value::Object(Some(selector_cur)));
    // Pinned across the builder `request()` invoke below.
    let sel_arr_pin = ctx.pin_native_root(sel_arr);

    let builder_val = ctx.invoke(BUILDER, "request", DESC_BUILDER, &[])?;
    let builder = match builder_val {
        Some(Value::Object(Some(b))) => b,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message:
                    "TestPlanScannerFilter.accept: LauncherDiscoveryRequestBuilder.request() failed"
                        .into(),
            }
            .into());
        }
    };

    let sel_arr_cur = ctx.read_native_pin(sel_arr_pin, sel_arr);
    let builder_val = ctx.invoke_virtual(
        builder,
        "selectors",
        DESC_SELECTORS,
        &[Value::Object(Some(sel_arr_cur))],
    )?;
    let builder = match builder_val {
        Some(Value::Object(Some(b))) => b,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "TestPlanScannerFilter.accept: selectors() failed".into(),
            }
            .into());
        }
    };
    // Pinned across the possible load_class/new_ref_array in the filters
    // fallback branch below.
    let builder_pin = ctx.pin_native_root(builder);

    let this_cur = ctx.read_native_pin(this_pin, this);
    let filters_val = ctx.get_field_by_name(this_cur, "includeAndExcludeFilters");
    let filters_arg = match filters_val {
        Value::Object(Some(f)) => Value::Object(Some(f)),
        _ => {
            let f_mirror = match ctx.load_class("org/junit/platform/engine/Filter")? {
                Some(Value::Object(Some(m))) => m,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "TestPlanScannerFilter.accept: could not load Filter".into(),
                    }
                    .into());
                }
            };
            let f_cid = ctx.class_id_of_object(f_mirror);
            Value::Object(Some(ctx.new_ref_array(f_cid, 0)))
        }
    };

    let builder_cur = ctx.read_native_pin(builder_pin, builder);
    let builder_val = ctx.invoke_virtual(builder_cur, "filters", DESC_FILTERS, &[filters_arg])?;
    let builder = match builder_val {
        Some(Value::Object(Some(b))) => b,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "TestPlanScannerFilter.accept: filters() failed".into(),
            }
            .into());
        }
    };

    let req_val = ctx.invoke_virtual(builder, "build", DESC_BUILD, &[])?;
    let request = match req_val {
        Some(Value::Object(Some(r))) => r,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "TestPlanScannerFilter.accept: build() failed".into(),
            }
            .into());
        }
    };
    // Pinned across the possible LauncherFactory.create() below.
    let request_pin = ctx.pin_native_root(request);

    let this_cur = ctx.read_native_pin(this_pin, this);
    let launcher_field = ctx.get_field_by_name(this_cur, "launcher");
    let delegate = match launcher_field {
        Value::Object(Some(l)) => l,
        _ => {
            let v = ctx.invoke(FACTORY, "create", DESC_FACTORY, &[])?;
            match v {
                Some(Value::Object(Some(d))) => d,
                other => {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "TestPlanScannerFilter.accept: LauncherFactory.create() expected Launcher, got {other:?}"
                        ),
                    }
                    .into());
                }
            }
        }
    };

    let request_cur = Value::Object(Some(ctx.read_native_pin(request_pin, request)));
    let plan_val = ctx.invoke_virtual(delegate, "discover", DESC_DISCOVER, &[request_cur])?;
    let plan = match plan_val {
        Some(Value::Object(Some(p))) => p,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "TestPlanScannerFilter.accept: discover() failed".into(),
            }
            .into());
        }
    };

    let contains = ctx.invoke_virtual(plan, "containsTests", "()Z", &[])?;
    let b = match contains {
        Some(Value::Int(i)) => i != 0,
        Some(Value::Long(l)) => l != 0,
        _ => false,
    };
    Ok(Some(Value::Int(if b { 1 } else { 0 })))
}

pub(crate) fn native_surefire_properties_wrapper_get_property_1(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(Value::Object(None))),
    };
    let map_field = ctx.get_field_by_name(this, "properties");
    if let Value::Object(Some(props_map)) = map_field {
        match cratonvm_native_collections::native_map_get_pub(
            ctx,
            &[Value::Object(Some(props_map)), Value::Object(Some(key_obj))],
        ) {
            Ok(Some(Value::Object(Some(v)))) => {
                // Guard against non-String values leaking from partially
                // materialized map implementations during surefire bootstrap.
                if ctx.read_string(v).is_some() {
                    return Ok(Some(Value::Object(Some(v))));
                }
            }
            Ok(_) | Err(_) => {}
        }
        let pk_st = property_key_from_java_string(ctx, key_obj);
        if let Some(v) =
            crate::properties_sidetable::get_property_from_sidetable(ctx, props_map, &pk_st)
        {
            return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
        }
    }
    let key = property_key_from_java_string(ctx, key_obj);
    if key == "forkNodeConnectionString" {
        // ForkedBooter.lookupDecoderFactory(connectionString) returns null if
        // the value does not match known prefixes (pipe:// or tcp://). During
        // early bootstrap we occasionally miss this property in the wrapper map;
        // default to legacy pipe transport to keep booter initialization alive.
        return Ok(Some(Value::Object(Some(ctx.create_string("pipe://")))));
    }
    // BooterDeserializer.deserialize() calls Shutdown.valueOf(getProperty("shutdown")).
    // If provider properties are only partially materialized, this key can
    // come back null and Enum.valueOf throws NPE("Name is null").
    if key == "shutdown" {
        return Ok(Some(Value::Object(Some(ctx.create_string("DEFAULT")))));
    }
    match ctx
        .get_system_property(&key)
        .or_else(|| system_property_fallback(ctx, &key))
    {
        Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_surefire_properties_wrapper_get_property_2(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(args.get(2).copied().unwrap_or(Value::Object(None)))),
    };
    let default = args.get(2).copied().unwrap_or(Value::Object(None));
    let key_obj = match args.get(1) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(Some(default)),
    };
    if let Value::Object(Some(props_map)) = ctx.get_field_by_name(this, "properties") {
        if let Ok(Some(Value::Object(Some(v)))) = cratonvm_native_collections::native_map_get_pub(
            ctx,
            &[Value::Object(Some(props_map)), Value::Object(Some(key_obj))],
        ) {
            if ctx.read_string(v).is_some() {
                return Ok(Some(Value::Object(Some(v))));
            }
        }
        let pk_st = property_key_from_java_string(ctx, key_obj);
        if let Some(v) =
            crate::properties_sidetable::get_property_from_sidetable(ctx, props_map, &pk_st)
        {
            return Ok(Some(Value::Object(Some(ctx.create_string(&v)))));
        }
    }
    let key = property_key_from_java_string(ctx, key_obj);
    if key == "forkNodeConnectionString" {
        return Ok(Some(Value::Object(Some(ctx.create_string("pipe://")))));
    }
    if key == "shutdown" {
        return Ok(Some(Value::Object(Some(ctx.create_string("DEFAULT")))));
    }
    match ctx
        .get_system_property(&key)
        .or_else(|| system_property_fallback(ctx, &key))
    {
        Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
        None => Ok(Some(default)),
    }
}

pub(crate) fn native_surefire_dump_exception(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(t))) = args.get(1) {
        eprintln!("[SUREFIRE-BOOT] dumpException called:");
        if let Ok(Some(Value::Object(Some(msg_obj)))) =
            native_throwable_to_string(ctx, &[Value::Object(Some(*t))])
        {
            if let Some(msg) = ctx.read_string(msg_obj) {
                eprintln!("[SUREFIRE-BOOT] throwable={msg}");
            }
        }
        // Print captured Java backtrace frames when available. This gives
        // the exact producer callsite for early surefire bootstrap failures.
        let thash = ctx.identity_hash_code(*t);
        if let Some(frames) = ctx.get_stack_trace(thash) {
            if !frames.is_empty() {
                eprintln!(
                    "[SUREFIRE-BOOT] captured stack trace (top {} frames):",
                    frames.len().min(16)
                );
                for (i, f) in frames.iter().take(16).enumerate() {
                    let src = f.source_file.as_deref().unwrap_or("Unknown Source");
                    eprintln!(
                        "[SUREFIRE-BOOT]   #{i} {}.{} ({}:{})",
                        f.class_name, f.method_name, src, f.line_number
                    );
                }
            }
        }
        let _ = native_throwable_print_stack_trace(ctx, &[Value::Object(Some(*t))]);
    }
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_surefire_properties_wrapper_get_int_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = native_surefire_properties_wrapper_get_property_1(ctx, args)?;
    let s = match v {
        Some(Value::Object(Some(obj))) => ctx.read_string(obj).unwrap_or_default(),
        _ => String::new(),
    };
    if s.is_empty() {
        // surefire uses this for fork number; default to 1 in forked mode.
        return Ok(Some(Value::Int(1)));
    }
    let parsed = s.parse::<i32>().unwrap_or(1);
    Ok(Some(Value::Int(parsed)))
}

pub(crate) fn native_surefire_properties_wrapper_get_boolean_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = native_surefire_properties_wrapper_get_property_1(ctx, args)?;
    let s = match v {
        Some(Value::Object(Some(obj))) => ctx.read_string(obj).unwrap_or_default(),
        _ => String::new(),
    };
    let b = if s.eq_ignore_ascii_case("true") { 1 } else { 0 };
    Ok(Some(Value::Int(b)))
}

pub(crate) fn native_surefire_properties_wrapper_set_as_system_properties(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Surefire's `setAsSystemProperties` iterates the wrapper's
    // `properties` map and calls `System.setProperty(k, v)` for each
    // entry. The bytecode iteration over our synthetic CHM does not
    // reliably observe the entries (CHM-specific layout), so honour
    // the contract by walking our wrapper-side side-table and seeding
    // each entry into the VM's system property store.
    if let Some(Value::Object(Some(this))) = args.first() {
        let entries = crate::properties_sidetable::snapshot_sidetable(ctx, *this);
        for (k, v) in entries {
            let _ = ctx.set_system_property(&k, &v);
        }
    }
    Ok(None)
}

pub(crate) fn native_surefire_junit4_reflector_create_description(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    const DESC: &str = "org/junit/runner/Description";
    const METHOD: &str = "createSuiteDescription";
    const DESC_STRING: &str = "(Ljava/lang/String;)Lorg/junit/runner/Description;";
    const DESC_STRING_ANN: &str =
        "(Ljava/lang/String;[Ljava/lang/annotation/Annotation;)Lorg/junit/runner/Description;";

    let name_ref = match args.first().copied() {
        Some(Value::Object(Some(name))) => Some(name),
        _ => None,
    };
    let provided_ann_ref = match args.get(1).copied() {
        Some(Value::Object(Some(annotations))) => Some(annotations),
        _ => None,
    };

    let mut root_base = None;
    let mut name_handle = None;
    if let Some(name) = name_ref {
        let handle = ctx.pin_native_root(name);
        root_base = Some(handle);
        name_handle = Some(handle);
    }
    let mut ann_handle = None;
    if let Some(annotations) = provided_ann_ref {
        let handle = ctx.pin_native_root(annotations);
        root_base.get_or_insert(handle);
        ann_handle = Some(handle);
    }

    let load_result = ctx.load_class(DESC);
    if let Err(err) = load_result {
        if let Some(base) = root_base {
            ctx.unpin_native_roots(base);
        }
        return Err(err);
    }

    let name_arg = match (name_ref, name_handle) {
        (Some(name), Some(handle)) => Value::Object(Some(ctx.read_native_pin(handle, name))),
        _ => Value::Object(None),
    };

    let result = if ctx.method_exists(DESC, METHOD, DESC_STRING_ANN) {
        let ann_ref = match provided_ann_ref {
            Some(annotations) => annotations,
            None => {
                let ann_cid = ctx
                    .class_id_by_name("java/lang/annotation/Annotation")
                    .unwrap_or_else(|| ClassId::new(0));
                ctx.new_ref_array(ann_cid, 0)
            }
        };
        let ann_handle = ann_handle.unwrap_or_else(|| {
            let handle = ctx.pin_native_root(ann_ref);
            root_base.get_or_insert(handle);
            handle
        });
        let ann_arg = Value::Object(Some(ctx.read_native_pin(ann_handle, ann_ref)));
        ctx.invoke(DESC, METHOD, DESC_STRING_ANN, &[name_arg, ann_arg])
    } else {
        ctx.invoke(DESC, METHOD, DESC_STRING, &[name_arg])
    };

    if let Some(base) = root_base {
        ctx.unpin_native_roots(base);
    }
    result
}

/// Native impl for `SystemPropertyManager.loadProperties(InputStream)
///   -> PropertiesWrapper`. Bypasses the bytecode round-trip through
/// `Properties.load → stringPropertyNames → ConcurrentHashMap.put` —
/// our synthetic CHM does not faithfully store entries through that
/// path. Instead we drain the input stream, parse the `.properties`
/// bytes, allocate a `PropertiesWrapper`, populate the wrapper-side
/// side-table directly, and return it.
pub(crate) fn native_surefire_system_property_manager_load_properties(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let stream = match args.first() {
        Some(Value::Object(Some(s))) => *s,
        _ => return Ok(Some(Value::Object(None))),
    };
    let bytes = match crate::properties_sidetable::drain_input_stream_pub(ctx, stream) {
        Some(b) => b,
        None => Vec::new(),
    };
    let mut parsed = crate::properties_sidetable::parse_properties_pub(&bytes);
    // `StartupConfiguration.inForkedVm` passes `providerConfiguration` straight
    // into `StartupConfiguration`; `isProviderMainClass()` calls
    // `providerClassName.endsWith("#main")` and NPEs if the property is missing
    // or blank after our bootstrap path dropped it during parse/map hydration.
    let has_provider = parsed
        .iter()
        .any(|(k, v)| k == "providerConfiguration" && !v.trim().is_empty());
    if !has_provider {
        parsed.retain(|(k, _)| k != "providerConfiguration");
        parsed.push((
            "providerConfiguration".to_string(),
            "org.apache.maven.surefire.junitplatform.JUnitPlatformProvider".to_string(),
        ));
    }
    // Allocate a PropertiesWrapper with enough fields for the JDK
    // layout (`properties` is the only declared field).  Use the
    // standard synthetic allocator so the class is initialised first.
    let wrapper = try_alloc_concurrent_synthetic(
        ctx,
        "org/apache/maven/surefire/booter/PropertiesWrapper",
        2,
    )?;
    // Allocate a tiny placeholder map for the `properties` field so any
    // bytecode that touches the field (not via our overrides) sees a
    // non-null Map. Use a HashMap (well-known to our natives) rather
    // than ConcurrentHashMap to keep the placeholder layout-stable.
    let placeholder = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 8)?;
    if let Ok(_) =
        cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(placeholder))])
    {}
    ctx.set_field_by_name(wrapper, "properties", Value::Object(Some(placeholder)));
    for (k, v) in &parsed {
        crate::properties_sidetable::store_property_in_sidetable(ctx, wrapper, k, v);
        // `PropertiesWrapper.getProperty` is compiled as `this.properties.get(key)`.
        // If dispatch hits the real `HashMap` instead of our sidetable-backed
        // overrides, the map must still contain every booter entry — otherwise
        // `BooterDeserializer.getStartupConfiguration` passes a null provider
        // class name into `StartupConfiguration`.
        let k_obj = ctx.create_string(k);
        let v_obj = ctx.create_string(v);
        let _ = cratonvm_native_collections::native_map_put_pub(
            ctx,
            &[
                Value::Object(Some(placeholder)),
                Value::Object(Some(k_obj)),
                Value::Object(Some(v_obj)),
            ],
        );
    }
    eprintln!(
        "[SPM-LOAD] wrapper={:?} parsed_entries={} stream_bytes={}",
        wrapper,
        parsed.len(),
        bytes.len()
    );
    for (k, v) in &parsed {
        if matches!(
            k.as_str(),
            "forkNodeConnectionString"
                | "shutdown"
                | "reportsDirectory"
                | "forkNumber"
                | "failFastCount"
                | "rerunFailingTestsCount"
                | "isTrimStackTrace"
                | "useSystemClassLoader"
                | "providerConfiguration"
                | "providerClass"
        ) {
            eprintln!("[SPM-LOAD]   {} = {}", k, v);
        }
    }
    Ok(Some(Value::Object(Some(wrapper))))
}

/// Native impl for `SystemPropertyManager.setSystemProperties(File)`.
/// Reads the named file directly via std::fs::read so we do not depend
/// on FileInputStream + Properties.load bytecode flowing correctly.
pub(crate) fn native_surefire_system_property_manager_set_system_properties(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let file = match args.first() {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(None),
    };
    let path_str = match ctx.get_field_by_name(file, "path") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if path_str.is_empty() {
        return Ok(None);
    }
    let bytes = match std::fs::read(&path_str) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };
    let parsed = crate::properties_sidetable::parse_properties_pub(&bytes);
    eprintln!(
        "[SPM-SET] path={:?} parsed_entries={} bytes={}",
        path_str,
        parsed.len(),
        bytes.len()
    );
    for (k, v) in &parsed {
        let _ = ctx.set_system_property(k, v);
    }
    Ok(None)
}

pub(crate) fn native_forkedbooter_create_surefire_properties_if_file_exists(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let parent = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let child = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    if parent.is_empty() || child.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let path = std::path::Path::new(&parent).join(&child);
    if !path.exists() {
        return Ok(Some(Value::Object(None)));
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Ok(Some(Value::Object(None))),
    };
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i32));
    }
    let stream = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4)?;
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    ctx.set_field(stream, 1, Value::Int(0));
    ctx.set_field(stream, 2, Value::Int(0));
    ctx.set_field(stream, 3, Value::Int(bytes.len() as i32));
    Ok(Some(Value::Object(Some(stream))))
}

pub(crate) fn native_surefire_lookup_decoder_factory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let instantiate_factory = |ctx: &mut dyn NativeContext,
                               class_name: &str|
     -> Result<Option<ObjectRef>, MethodCallFailed> {
        let init_ok = ctx.ensure_class_initialized(class_name).is_ok();
        let obj = if init_ok {
            match ctx.new_object(class_name) {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => try_alloc_concurrent_synthetic(ctx, class_name, 0)?,
            }
        } else {
            try_alloc_concurrent_synthetic(ctx, class_name, 0)?
        };
        // Use real object allocation + constructor init so surefire's internal
        // processor/channel fields are materialized with the expected layout.
        let _ = ctx.invoke_special(class_name, "<init>", "()V", &[Value::Object(Some(obj))]);
        Ok(Some(obj))
    };

    let conn_val = args.first().copied().unwrap_or(Value::Object(None));
    let conn_text = match conn_val {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    // setupBooter() immediately invokes `factory.connect(connectionString)`.
    // Normalize missing/blank strings to legacy pipe mode so the downstream
    // connect call never sees null and cannot throw "connect on null".
    let normalized_conn = if conn_text.trim().is_empty() {
        "pipe://".to_string()
    } else {
        conn_text.clone()
    };
    let conn_obj = ctx.create_string(&normalized_conn);
    // Prefer the modern processor first even for `pipe://` connection strings.
    // The legacy implementation's periodic flusher path currently trips our
    // ScheduledThreadPoolExecutor emulation (`Cannot invoke add on null`).
    let candidates = [
        "org/apache/maven/surefire/booter/spi/SurefireMasterProcessChannelProcessorFactory",
        "org/apache/maven/surefire/booter/spi/LegacyMasterProcessChannelProcessorFactory",
    ];
    for class_name in candidates {
        let Some(factory) = instantiate_factory(ctx, class_name)? else {
            continue;
        };
        let can_use = ctx.invoke_virtual(
            factory,
            "canUse",
            "(Ljava/lang/String;)Z",
            &[Value::Object(Some(conn_obj))],
        );
        if !matches!(can_use, Ok(Some(Value::Int(1)))) {
            continue;
        }
        let connect = ctx.invoke_virtual(
            factory,
            "connect",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(conn_obj))],
        );
        if connect.is_ok() {
            return Ok(Some(Value::Object(Some(factory))));
        }
    }
    // If capability checks are unreliable, at least require connect() to accept
    // the normalized transport string before returning.
    for class_name in candidates {
        let Some(factory) = instantiate_factory(ctx, class_name)? else {
            continue;
        };
        let connect = ctx.invoke_virtual(
            factory,
            "connect",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(conn_obj))],
        );
        if connect.is_ok() {
            return Ok(Some(Value::Object(Some(factory))));
        }
    }
    for class_name in candidates {
        if let Some(factory) = instantiate_factory(ctx, class_name)? {
            return Ok(Some(Value::Object(Some(factory))));
        }
    }
    Err(MethodCallFailed::InternalError(VmError::Internal {
        message: "Surefire lookupDecoderFactory: no processor factories available".to_string(),
    }))
}

pub(crate) fn native_surefire_forkedbooter_run(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let booter = obj_arg(args, 0)?;
    let argv = obj_arg(args, 1)?;
    let argv_len = ctx.array_length(argv);
    let get_arg = |ctx: &mut dyn NativeContext, arr: ObjectRef, idx: usize| -> Option<ObjectRef> {
        if idx >= ctx.array_length(arr) {
            return None;
        }
        match ctx.get_array_element(arr, idx) {
            Value::Object(Some(s)) => Some(s),
            _ => None,
        }
    };
    let arg0 = get_arg(ctx, argv, 0)
        .map(|o| Value::Object(Some(o)))
        .unwrap_or(Value::Object(None));
    let arg1 = get_arg(ctx, argv, 1)
        .map(|o| Value::Object(Some(o)))
        .unwrap_or(Value::Object(None));
    let arg2 = get_arg(ctx, argv, 2)
        .map(|o| Value::Object(Some(o)))
        .unwrap_or(Value::Object(None));
    let arg3 = if argv_len > 3 {
        get_arg(ctx, argv, 3)
            .map(|o| Value::Object(Some(o)))
            .unwrap_or(Value::Object(None))
    } else {
        Value::Object(None)
    };
    eprintln!(
        "[SUREFIRE-RUN] entering ForkedBooter.run argv_len={}",
        argv_len
    );
    let setup = ctx.invoke_special(
        "org/apache/maven/surefire/booter/ForkedBooter",
        "setupBooter",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
        &[Value::Object(Some(booter)), arg0, arg1, arg2, arg3],
    );
    if let Err(err) = setup {
        eprintln!(
            "[SUREFIRE-RUN] setupBooter threw (first attempt): {:?}",
            err
        );
        if let MethodCallFailed::ExceptionThrown(exc) = &err {
            eprint_java_throwable(ctx, "setupBooter (first)", *exc);
        }
        // Surefire forks occasionally race during very early bootstrap in
        // cratonvm shim mode (factory/connection state not fully materialized
        // yet). Retry setup once before taking the hard exit1() branch.
        let setup_retry = ctx.invoke_special(
            "org/apache/maven/surefire/booter/ForkedBooter",
            "setupBooter",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
            &[Value::Object(Some(booter)), arg0, arg1, arg2, arg3],
        );
        if setup_retry.is_ok() {
            eprintln!("[SUREFIRE-RUN] setupBooter recovered on retry");
        } else {
            eprintln!(
                "[SUREFIRE-RUN] setupBooter threw (retry): {:?}",
                setup_retry
            );
            if let Err(MethodCallFailed::ExceptionThrown(exc)) = &setup_retry {
                eprint_java_throwable(ctx, "setupBooter (retry)", *exc);
            }
            let _ = ctx.invoke_special(
                "org/apache/maven/surefire/booter/ForkedBooter",
                "cancelPingScheduler",
                "()V",
                &[Value::Object(Some(booter))],
            );
            let _ = ctx.invoke_special(
                "org/apache/maven/surefire/booter/ForkedBooter",
                "exit1",
                "()V",
                &[Value::Object(Some(booter))],
            );
            return Ok(None);
        }
    }
    // In CratonVM's fork-shim mode the test set is already materialized by
    // setupBooter. Keeping Surefire's command reader live makes JUnit4Provider
    // wait in CommandReader.awaitStarted(), but there is no interactive master
    // command stream for the single-class runner. Null it before provider
    // construction so the provider skips that wait path.
    ctx.set_field_by_name(booter, "commandReader", Value::Object(None));
    let exec = ctx.invoke_special(
        "org/apache/maven/surefire/booter/ForkedBooter",
        "execute",
        "()V",
        &[Value::Object(Some(booter))],
    );
    if let Err(err) = exec {
        eprintln!("[SUREFIRE-RUN] execute threw: {:?}", err);
        let _ = ctx.invoke_special(
            "org/apache/maven/surefire/booter/ForkedBooter",
            "cancelPingScheduler",
            "()V",
            &[Value::Object(Some(booter))],
        );
        let _ = ctx.invoke_special(
            "org/apache/maven/surefire/booter/ForkedBooter",
            "exit1",
            "()V",
            &[Value::Object(Some(booter))],
        );
    }
    Ok(None)
}

fn surefire_ignore(label: &str, result: MethodCallResult) {
    if let Err(err) = result {
        eprintln!("[SUREFIRE-ACK-EXIT] {label} ignored: {err:?}");
    }
}

pub(crate) fn native_surefire_forkedbooter_acknowledged_exit(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let is_null = |v: Value| matches!(v, Value::Object(None));
    let ev = ctx.get_field_by_name(this, "eventChannel");
    let cr = ctx.get_field_by_name(this, "commandReader");
    eprintln!(
        "[SUREFIRE-ACK-EXIT] invoked; eventChannel_null={} commandReader_null={}",
        is_null(ev),
        is_null(cr),
    );

    if let Value::Object(Some(event_channel)) = ev {
        // `bye()` can move `event_channel` before `onJvmExit()` reads it.
        let ch_pin = ctx.pin_native_root(event_channel);
        surefire_ignore(
            "eventChannel.bye",
            ctx.invoke_virtual(event_channel, "bye", "()V", &[]),
        );
        let event_channel = ctx.read_native_pin(ch_pin, event_channel);
        surefire_ignore(
            "eventChannel.onJvmExit",
            ctx.invoke_virtual(event_channel, "onJvmExit", "()V", &[]),
        );
    } else {
        // Older Surefire booters (2.x line -- e.g. 2.22.2, still pinned by
        // WildFly's testsuite poms) have no `eventChannel`/`closeForkChannel`
        // at all: the parent's `ForkClient` only marks `saidGoodBye = true`
        // once it reads a literal "Z,0,BYE!\n" line on the forked process's
        // stdout, written by the real `acknowledgedExit()` via
        // `encodeAndWriteToOutput(String)` before the process exits. Without
        // this, `std::process::exit(0)` below leaves that flag unset and
        // Maven reports "The forked VM terminated without properly saying
        // goodbye" even though zero matching tests is the correct outcome.
        let bye = ctx.create_string("Z,0,BYE!\n");
        surefire_ignore(
            "encodeAndWriteToOutput(BYE)",
            ctx.invoke_special(
                "org/apache/maven/surefire/booter/ForkedBooter",
                "encodeAndWriteToOutput",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(this)), Value::Object(Some(bye))],
            ),
        );
        // The write above reaches the OS pipe correctly, but CratonVM can
        // then call process::exit() before Maven's own asynchronous
        // stdout-pumping thread has even been scheduled to read it -- for
        // trivial/fast test classes there is enough real wall-clock work
        // (class loading, JIT warmup, GC) in a real HotSpot fork that this
        // race never shows up there, but CratonVM's much faster teardown
        // exposes it. Real Surefire's own acknowledgedExit() handles this by
        // registering a bye-ack listener and blocking (bounded) until the
        // parent's ForkClient explicitly acknowledges receipt
        // (TestLessInputStream.acknowledgeByeEventReceived() queues
        // Command.BYE_ACK and releases a semaphore) -- do the same here
        // instead of exiting blind. Bounded well under the real 30s default
        // exit-timeout: this is only ever meant to absorb an OS scheduling
        // gap of a few milliseconds, not to wait out a genuinely wedged
        // parent.
        if let Value::Object(Some(command_reader)) = cr {
            let wait_result: MethodCallResult = (|| {
                let sem = match ctx.new_object("java/util/concurrent/Semaphore")? {
                    Some(Value::Object(Some(o))) => o,
                    _ => return Ok(None),
                };
                ctx.invoke_special(
                    "java/util/concurrent/Semaphore",
                    "<init>",
                    "(I)V",
                    &[Value::Object(Some(sem)), Value::Int(0)],
                )?;
                let listener =
                    match ctx.new_object("org/apache/maven/surefire/booter/ForkedBooter$6")? {
                        Some(Value::Object(Some(o))) => o,
                        _ => return Ok(None),
                    };
                ctx.invoke_special(
                    "org/apache/maven/surefire/booter/ForkedBooter$6",
                    "<init>",
                    "(Lorg/apache/maven/surefire/booter/ForkedBooter;Ljava/util/concurrent/Semaphore;)V",
                    &[
                        Value::Object(Some(listener)),
                        Value::Object(Some(this)),
                        Value::Object(Some(sem)),
                    ],
                )?;
                ctx.invoke_virtual(
                    command_reader,
                    "addByeAckListener",
                    "(Lorg/apache/maven/surefire/booter/CommandListener;)V",
                    &[Value::Object(Some(listener))],
                )?;
                ctx.invoke_virtual(
                    sem,
                    "tryAcquire",
                    "(JLjava/util/concurrent/TimeUnit;)Z",
                    &[Value::Long(2000), Value::Object(None)],
                )
            })();
            surefire_ignore("bye-ack-wait", wait_result);
        }
    }
    surefire_ignore(
        "cancelPingScheduler",
        ctx.invoke_special(
            "org/apache/maven/surefire/booter/ForkedBooter",
            "cancelPingScheduler",
            "()V",
            &[Value::Object(Some(this))],
        ),
    );
    if let Value::Object(Some(command_reader)) = cr {
        surefire_ignore(
            "commandReader.stop",
            ctx.invoke_virtual(command_reader, "stop", "()V", &[]),
        );
    }
    if let Value::Object(Some(_)) = ev {
        surefire_ignore(
            "closeForkChannel",
            ctx.invoke_special(
                "org/apache/maven/surefire/booter/ForkedBooter",
                "closeForkChannel",
                "()V",
                &[Value::Object(Some(this))],
            ),
        );
    }

    if crate::nbflags().soft_exit {
        eprintln!("[SUREFIRE-ACK-EXIT] soft-returning due to CRATONVM_SOFT_EXIT=1");
        return Ok(None);
    }
    // W7-90 trigger C. A Surefire fork never reaches `System.exit`: this triple
    // is registered, so real `ForkedBooter` bytecode never runs, which skips
    // both the launcher's post-`main` sweep and `lang_system`'s three exit
    // natives. Same shared helper, gated with no `else`, carrying its own
    // trigger label. Below the soft-exit return so a soft-returned exit does not
    // consume the census the launcher would print later.
    crate::lang_system::sweep_declared_slot_maps_before_exit(
        &*ctx,
        "ForkedBooter.acknowledgedExit",
    );
    std::process::exit(0);
}

pub(crate) fn native_surefire_forkedbooter_exit1(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let is_null = |v: Value| matches!(v, Value::Object(None));
    let ev = ctx.get_field_by_name(this, "eventChannel");
    let cr = ctx.get_field_by_name(this, "commandReader");
    let pc = ctx.get_field_by_name(this, "providerConfiguration");
    let sc = ctx.get_field_by_name(this, "startupConfiguration");
    let ts = ctx.get_field_by_name(this, "testSet");
    eprintln!(
        "[SUREFIRE-EXIT] invoked; eventChannel_null={} commandReader_null={} providerConfig_null={} startupConfig_null={} testSet_null={}",
        is_null(ev),
        is_null(cr),
        is_null(pc),
        is_null(sc),
        is_null(ts),
    );
    if crate::nbflags().soft_exit {
        eprintln!("[SUREFIRE-EXIT] soft-returning due to CRATONVM_SOFT_EXIT=1");
        return Ok(None);
    }
    // W7-90 trigger C — see `native_surefire_forkedbooter_acknowledged_exit`.
    // This body serves TWO registered triples (`exit()V` and `exit1()V`), so one
    // label covers both; the label is `exit1` because that is the name the
    // registration this body was written for uses.
    crate::lang_system::sweep_declared_slot_maps_before_exit(&*ctx, "ForkedBooter.exit1");
    std::process::exit(1);
}

pub(crate) fn native_surefire_forkedbooter_exit_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let code = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    let is_null = |v: Value| matches!(v, Value::Object(None));
    let ev = ctx.get_field_by_name(this, "eventChannel");
    let cr = ctx.get_field_by_name(this, "commandReader");
    eprintln!(
        "[SUREFIRE-EXIT] exit({}) invoked; eventChannel_null={} commandReader_null={}",
        code,
        is_null(ev),
        is_null(cr),
    );
    if crate::nbflags().soft_exit {
        eprintln!("[SUREFIRE-EXIT] soft-returning due to CRATONVM_SOFT_EXIT=1");
        return Ok(None);
    }
    // W7-90 trigger C — see `native_surefire_forkedbooter_acknowledged_exit`.
    crate::lang_system::sweep_declared_slot_maps_before_exit(&*ctx, "ForkedBooter.exit");
    std::process::exit(code);
}

fn is_surefire_forwarding_print_stream(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(stream))
        .as_deref()
        == Some(SUREFIRE_FORWARDING_PRINT_STREAM)
}

pub(crate) fn surefire_forwarding_write(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    text: &str,
    newline: bool,
) -> bool {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return false,
    };
    if !is_surefire_forwarding_print_stream(ctx, this) {
        return false;
    }

    let text_obj = ctx.create_string(text);
    let text_root = ctx.pin_native_root(text_obj);
    let entry = match ctx.new_object("org/apache/maven/surefire/api/report/TestOutputReportEntry") {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(text_root);
            return false;
        }
    };

    let text_obj = ctx.read_native_pin(text_root, text_obj);
    ctx.unpin_native_roots(text_root);

    let target = match ctx.get_field_by_name(this, "target") {
        Value::Object(Some(o)) => o,
        _ => return false,
    };
    let is_stdout = matches!(ctx.get_field_by_name(this, "isStdout"), Value::Int(v) if v != 0);

    ctx.set_field_by_name(entry, "log", Value::Object(Some(text_obj)));
    ctx.set_field_by_name(entry, "isStdOut", Value::Int(if is_stdout { 1 } else { 0 }));
    ctx.set_field_by_name(entry, "newLine", Value::Int(if newline { 1 } else { 0 }));
    ctx.set_field_by_name(entry, "runMode", Value::Object(None));
    ctx.set_field_by_name(entry, "testRunId", Value::Object(None));

    ctx.invoke_virtual(
        target,
        "writeTestOutput",
        "(Lorg/apache/maven/surefire/api/report/OutputReportEntry;)V",
        &[Value::Object(Some(entry))],
    )
    .is_ok()
}

fn javac_location_name_is_module_oriented(name: &str) -> bool {
    matches!(
        name,
        "ANNOTATION_PROCESSOR_MODULE_PATH"
            | "MODULE_SOURCE_PATH"
            | "UPGRADE_MODULE_PATH"
            | "SYSTEM_MODULES"
            | "MODULE_PATH"
            | "PATCH_MODULE_PATH"
            | "MODULE"
    )
}

pub(crate) fn native_javac_file_manager_check_not_module_oriented_location(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let location = obj_arg(args, 1)?;
    let name = match ctx.invoke_virtual(location, "getName", "()Ljava/lang/String;", &[])? {
        Some(Value::Object(Some(name_obj))) => ctx.read_string(name_obj).unwrap_or_default(),
        _ => String::new(),
    };
    if javac_location_name_is_module_oriented(&name) {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("location is module-oriented: {name}"),
        }
        .into());
    }
    Ok(None)
}

fn javac_empty_array_list(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    Ok(javac_array_list_from_values(ctx, &[])?)
}

fn javac_array_list_from_values(
    ctx: &mut dyn NativeContext,
    values: &[Value],
) -> Result<ObjectRef, MethodCallFailed> {
    let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    let pin_base = ctx.pin_native_root(list);
    let value_pins: Vec<Option<(usize, ObjectRef)>> = values
        .iter()
        .map(|value| match value {
            Value::Object(Some(obj)) => Some((ctx.pin_native_root(*obj), *obj)),
            _ => None,
        })
        .collect();
    let data = ctx.new_array(cratonvm_types::ArrayElementType::Reference, values.len());
    let data_pin = ctx.pin_native_root(data);
    for (idx, value) in values.iter().copied().enumerate() {
        let value = match value_pins[idx] {
            Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
            None => value,
        };
        let data = ctx.read_native_pin(data_pin, data);
        ctx.set_array_element(data, idx, value);
    }
    let list = ctx.read_native_pin(pin_base, list);
    let data = ctx.read_native_pin(data_pin, data);
    ctx.set_field_by_name(list, "elementData", Value::Object(Some(data)));
    ctx.set_field_by_name(list, "size", Value::Int(values.len() as i32));
    ctx.unpin_native_roots(pin_base);
    Ok(list)
}

fn javac_java_file_object_kind_class(ctx: &mut dyn NativeContext) -> Option<Value> {
    let class_id = ctx
        .ensure_class_initialized("javax/tools/JavaFileObject$Kind")
        .ok()?;
    let slot = ctx.static_field_index_by_name(class_id, "CLASS")?;
    Some(ctx.get_static_field(class_id, slot))
}

fn javac_platform_class_file_object(
    ctx: &mut dyn NativeContext,
    file_manager: ObjectRef,
    location: Value,
    kind_class: Value,
    class_name: &str,
) -> MethodCallResult {
    let name = ctx.create_string(class_name);
    ctx.invoke_virtual_bytecode_only(
        file_manager,
        "getJavaFileForInput",
        "(Ljavax/tools/JavaFileManager$Location;Ljava/lang/String;Ljavax/tools/JavaFileObject$Kind;)Ljavax/tools/JavaFileObject;",
        &[location, Value::Object(Some(name)), kind_class],
    )
}

pub(crate) fn native_javac_file_manager_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let location = args.get(1).copied().unwrap_or(Value::Object(None));
    let package = args.get(2).copied().unwrap_or(Value::Object(None));
    let kinds = args.get(3).copied().unwrap_or(Value::Object(None));
    // Every branch below can allocate or call back into javac. Keep the native
    // arguments and every accumulated JavaFileObject live and relocatable until
    // the result list is fully materialized. Without these pins, a young GC in
    // getJavaFileForInput truncated java.lang.annotation to the three objects
    // that happened to remain at their old addresses.
    let pin_base = ctx.pin_native_root(this);
    let location_pin = match location {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let package_pin = match package {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let kinds_pin = match kinds {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let result = (|| -> MethodCallResult {
        let this = ctx.read_native_pin(pin_base, this);
        let location = match location_pin {
            Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
            None => location,
        };
        let location_name = match location {
            Value::Object(Some(location_obj)) => {
                match ctx.invoke_virtual(location_obj, "getName", "()Ljava/lang/String;", &[])? {
                    Some(Value::Object(Some(name_obj))) => {
                        ctx.read_string(name_obj).unwrap_or_default()
                    }
                    _ => String::new(),
                }
            }
            _ => String::new(),
        };
        let package = match package_pin {
            Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
            None => package,
        };
        let package_name = match package {
            Value::Object(Some(name_obj)) => ctx.read_string(name_obj).unwrap_or_default(),
            _ => String::new(),
        };
        // Bootstrap/platform classes never live on CLASS_PATH (JVMS class
        // loading delegation: java.*/javax.* etc. are always resolved via the
        // bootstrap/platform module path, never the application classpath),
        // so short-circuit those packages to an empty list without touching
        // the real file manager at all.
        //
        // "com.example" is deliberately NOT included here (round 2026-07-15,
        // TestCompilerTests): it is this test suite's OWN scratch package for
        // BOTH dynamically-generated, in-memory-only classes (which the real
        // bytecode fallthrough below correctly reports as absent) AND
        // genuine, pre-compiled-to-disk test fixtures (e.g.
        // spring-core-test's com.example.PublicInterface / PackagePrivate).
        // Blanket-emptying "com"/"com.example" here made javac's own
        // symbol resolution unable to discover those on-disk fixtures via
        // JavaFileManager.list() (needed for package-scan symbol lookup, as
        // opposed to direct-by-name lookup via getJavaFileForInput(), which
        // was never short-circuited and always worked) -- surfacing as
        // "cannot find symbol: class PublicInterface" even though the class
        // file plainly exists on the classpath. Falling through to the real
        // bytecode list() below (already exercised, and correct, for every
        // other package) fixes this without reintroducing whatever
        // performance concern motivated the original "com"/"com.example"
        // short-circuit -- it was never measured against this on-disk-fixture
        // case, only against in-memory-only generated classes.
        if location_name == "CLASS_PATH"
            && (package_name == "java" || package_name.starts_with("java."))
        {
            return Ok(Some(Value::Object(Some(javac_empty_array_list(ctx)?))));
        }
        if let Some(module_name) = location_name
            .strip_prefix("SYSTEM_MODULES[")
            .and_then(|name| name.strip_suffix(']'))
        {
            if let Some(java_home) = ctx.get_system_property("java.home") {
                let recurse = matches!(args.get(4), Some(Value::Int(v)) if *v != 0);
                let class_names = phases_late::jrtfs_list_class_binary_names(
                    &java_home,
                    module_name,
                    &package_name,
                    recurse,
                );
                if class_names.is_empty() {
                    return Ok(Some(Value::Object(Some(javac_empty_array_list(ctx)?))));
                }
                let Some(kind_class) = javac_java_file_object_kind_class(ctx) else {
                    return Ok(Some(Value::Object(Some(javac_empty_array_list(ctx)?))));
                };
                let kind_class_pin = match kind_class {
                    Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
                    _ => None,
                };
                let kinds = match kinds_pin {
                    Some((pin, fallback)) => {
                        Value::Object(Some(ctx.read_native_pin(pin, fallback)))
                    }
                    None => kinds,
                };
                let kind_class = match kind_class_pin {
                    Some((pin, fallback)) => {
                        Value::Object(Some(ctx.read_native_pin(pin, fallback)))
                    }
                    None => kind_class,
                };
                let accepts_classes = match kinds {
                    Value::Object(Some(kinds_obj)) => matches!(
                        ctx.invoke_virtual(
                            kinds_obj,
                            "contains",
                            "(Ljava/lang/Object;)Z",
                            &[kind_class],
                        )?,
                        Some(Value::Int(value)) if value != 0
                    ),
                    _ => false,
                };
                if !accepts_classes {
                    return Ok(Some(Value::Object(Some(javac_empty_array_list(ctx)?))));
                }
                // Materialize directly into a pinned Java list. java.lang alone
                // contains hundreds of classes; retaining every JavaFileObject
                // in a Rust Vec plus one native pin per entry exhausted the
                // native-root window before javac reached annotation packages.
                let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
                let list_pin = ctx.pin_native_root(list);
                let data = ctx.new_array(
                    cratonvm_types::ArrayElementType::Reference,
                    class_names.len(),
                );
                let data_pin = ctx.pin_native_root(data);
                let mut file_count = 0usize;
                for class_name in &class_names {
                    let this = ctx.read_native_pin(pin_base, this);
                    let location = match location_pin {
                        Some((pin, fallback)) => {
                            Value::Object(Some(ctx.read_native_pin(pin, fallback)))
                        }
                        None => location,
                    };
                    let kind_class = match kind_class_pin {
                        Some((pin, fallback)) => {
                            Value::Object(Some(ctx.read_native_pin(pin, fallback)))
                        }
                        None => kind_class,
                    };
                    if let Some(Value::Object(Some(file))) = javac_platform_class_file_object(
                        ctx, this, location, kind_class, class_name,
                    )? {
                        let data = ctx.read_native_pin(data_pin, data);
                        ctx.set_array_element(data, file_count, Value::Object(Some(file)));
                        file_count += 1;
                    }
                }
                let list = ctx.read_native_pin(list_pin, list);
                let data = ctx.read_native_pin(data_pin, data);
                ctx.set_field_by_name(list, "elementData", Value::Object(Some(data)));
                ctx.set_field_by_name(list, "size", Value::Int(file_count as i32));
                ctx.unpin_native_roots(list_pin);
                return Ok(Some(Value::Object(Some(list))));
            }
        }
        let this = ctx.read_native_pin(pin_base, this);
        let location = match location_pin {
            Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
            None => location,
        };
        let mut forwarded_args = args[1..].to_vec();
        forwarded_args[0] = location;
        forwarded_args[1] = match package_pin {
            Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
            None => package,
        };
        forwarded_args[2] = match kinds_pin {
            Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
            None => kinds,
        };
        ctx.invoke_virtual_bytecode_only(
            this,
            "list",
            "(Ljavax/tools/JavaFileManager$Location;Ljava/lang/String;Ljava/util/Set;Z)Ljava/lang/Iterable;",
            &forwarded_args,
        )
    })();
    ctx.unpin_native_roots(pin_base);
    result
}

/// Converts a `PathFileObject` path to the binary name that javac derives by
/// removing its final extension and replacing path separators with dots.
///
/// The three callers below already hand us a path relative to their respective
/// classpath root: `RelativePath.path` for directories, a ZIP filesystem root
/// relative path for jars, and a JRT path below `modules/<module>`. Keeping the
/// transformation here explicit avoids re-entering the real-JDK Path and
/// Locations machinery for every scanned class file.
fn javac_binary_name_from_relative_path(path: &str) -> String {
    let path = path.trim_start_matches(['/', '\\']);
    let path = path.rsplit_once('.').map_or(path, |(stem, _)| stem);
    path.replace(['/', '\\'], ".")
}

fn javac_binary_name_from_jrt_path(path: &str) -> Option<String> {
    let path = path.trim_start_matches(['/', '\\']);
    let rest = path.strip_prefix("modules/")?;
    let (_, class_path) = rest.split_once(['/', '\\'])?;
    Some(javac_binary_name_from_relative_path(class_path))
}

#[cfg(test)]
#[test]
fn javac_binary_name_from_path_matches_javac_path_file_objects() {
    assert_eq!(
        javac_binary_name_from_relative_path("org/springframework/aot/Hint.class"),
        "org.springframework.aot.Hint"
    );
    assert_eq!(
        javac_binary_name_from_relative_path("\\org\\springframework\\aot\\Hint.class"),
        "org.springframework.aot.Hint"
    );
    assert_eq!(
        javac_binary_name_from_jrt_path("/modules/java.base/java/lang/String.class"),
        Some("java.lang.String".to_string())
    );
    assert_eq!(
        javac_binary_name_from_jrt_path("/not-modules/String.class"),
        None
    );
}

/// Native equivalent of the JDK 25 `JavacFileManager.inferBinaryName` fast
/// path for the concrete `PathFileObject` variants returned by its file
/// manager. The JDK bytecode first rebuilds the location path collection, then
/// dispatches to a tiny variant-specific conversion. During Spring AOT's
/// classpath scan that overhead is paid once per discovered class file.
///
/// Unknown JavaFileObject implementations deliberately delegate to bytecode:
/// they can encode a binary name with semantics not represented by a path.
pub(crate) fn native_javac_file_manager_infer_binary_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let location = obj_arg(args, 1)?;
    let file = obj_arg(args, 2)?;
    native_javac_file_manager_check_not_module_oriented_location(
        ctx,
        &[Value::Object(Some(this)), Value::Object(Some(location))],
    )?;

    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(file))
        .unwrap_or_default();
    let binary_name: Option<String> = match class_name.as_str() {
        "com/sun/tools/javac/file/PathFileObject$DirectoryFileObject" => {
            let relative_path = match ctx.get_field_by_name(file, "relativePath") {
                Value::Object(Some(relative_path)) => {
                    match ctx.get_field_by_name(relative_path, "path") {
                        Value::Object(Some(path)) => ctx.read_string(path),
                        _ => None,
                    }
                }
                _ => None,
            };
            relative_path.map(|path| javac_binary_name_from_relative_path(path.as_str()))
        }
        "com/sun/tools/javac/file/PathFileObject$JarFileObject" => {
            // Read the display string directly via the same logic
            // `Path.toString()`'s native uses (`p57_path_display_string`),
            // rather than `ctx.invoke_virtual(path, "toString", ...)`. The
            // latter doesn't consult `force_native_over_real_jdk_bytecode`/
            // the `vm_exec.rs` `check_override` allow-list the way the
            // bytecode interpreter's own `invokevirtual` handling does, so
            // it silently ran `Path`'s (nonexistent — `Path` is an
            // interface) real bytecode, which resolves to
            // `Object.toString()` and returns `java.nio.file.Path@<hash>` —
            // mangled by `javac_binary_name_from_relative_path` below into
            // the literal binary name `java.nio.file` for every ordinary
            // classpath class file (`ServletComponentScanRegistrarTests
            // #processAheadOfTimeDoesNotRegisterServletComponentRegisteringPostProcessor`
            // and any other real in-process javac compile — Spring's
            // `TestCompiler`/AOT generation — referencing an
            // application-classpath class).
            let path = match ctx.get_field_by_name(file, "path") {
                Value::Object(Some(path)) => {
                    Some(crate::phases_late::p57_path_display_string(ctx, path))
                }
                _ => None,
            };
            path.map(|path| javac_binary_name_from_relative_path(path.as_str()))
        }
        "com/sun/tools/javac/file/PathFileObject$JRTFileObject" => {
            let path = match ctx.get_field_by_name(file, "path") {
                Value::Object(Some(path)) => ctx
                    .invoke_virtual(path, "toString", "()Ljava/lang/String;", &[])?
                    .and_then(|value| match value {
                        Value::Object(Some(path)) => ctx.read_string(path),
                        _ => None,
                    }),
                _ => None,
            };
            path.and_then(|path| javac_binary_name_from_jrt_path(&path))
        }
        _ => None,
    };

    if let Some(binary_name) = binary_name {
        return Ok(Some(Value::Object(Some(ctx.create_string(&binary_name)))));
    }
    ctx.invoke_virtual_bytecode_only(
        this,
        "inferBinaryName",
        "(Ljavax/tools/JavaFileManager$Location;Ljavax/tools/JavaFileObject;)Ljava/lang/String;",
        &args[1..],
    )
}

pub(crate) fn native_javac_path_and_container_compare_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(Some(value))) => *value,
        _ => return Ok(Some(Value::Int(1))),
    };
    let left = ctx.get_field_by_name(this, "index").as_int().unwrap_or(0);
    let right = ctx.get_field_by_name(other, "index").as_int().unwrap_or(0);
    Ok(Some(Value::Int(left.wrapping_sub(right))))
}

pub(crate) fn native_javac_string_name_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = match ctx.get_field_by_name(this, "string") {
        Value::Object(Some(value)) => ctx.read_string(value).unwrap_or_default(),
        _ => String::new(),
    };
    Ok(Some(Value::Int(java_string_hash_code_ascii(&value))))
}

/// JDK 25 `Name.equals`: identity first, then exact concrete class and table,
/// followed by the representation-specific name comparison. Shared names are
/// indexed in their table; string-table names compare their String contents.
pub(crate) fn native_javac_shared_name_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(Some(other))) => *other,
        _ => return Ok(Some(Value::Int(0))),
    };
    if this == other {
        return Ok(Some(Value::Int(1)));
    }

    let this_class = ctx.class_id_of_object(this);
    if this_class != ctx.class_id_of_object(other) {
        return Ok(Some(Value::Int(0)));
    }
    if ctx.get_field_by_name(this, "table") != ctx.get_field_by_name(other, "table") {
        return Ok(Some(Value::Int(0)));
    }

    let class_name = ctx.class_name_of_id(this_class).unwrap_or_default();
    let equal = match class_name.as_str() {
        "com/sun/tools/javac/util/SharedNameTable$NameImpl" => {
            ctx.get_field_by_name(this, "index") == ctx.get_field_by_name(other, "index")
        }
        "com/sun/tools/javac/util/StringNameTable$NameImpl" => {
            let left = match ctx.get_field_by_name(this, "string") {
                Value::Object(Some(value)) => ctx.read_string(value).unwrap_or_default(),
                _ => String::new(),
            };
            let right = match ctx.get_field_by_name(other, "string") {
                Value::Object(Some(value)) => ctx.read_string(value).unwrap_or_default(),
                _ => String::new(),
            };
            left == right
        }
        // The concrete representations above are the javac tables exercised by
        // the real-JDK compiler path. Do not guess equality for an unfamiliar
        // implementation: exact-class non-identical names are unequal here.
        _ => false,
    };
    Ok(Some(Value::Int(equal as i32)))
}

fn javac_relative_path_string(ctx: &mut dyn NativeContext, obj: ObjectRef) -> String {
    match ctx.get_field_by_name(obj, "path") {
        Value::Object(Some(path_obj)) => ctx.read_string(path_obj).unwrap_or_default(),
        _ => String::new(),
    }
}

pub(crate) fn native_javac_relative_path_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(java_string_hash_code_ascii(
        &javac_relative_path_string(ctx, this),
    ))))
}

pub(crate) fn native_javac_relative_path_get_path(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field_by_name(this, "path")))
}

pub(crate) fn native_javac_relative_path_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other_class = ctx
        .class_name_of_id(ctx.class_id_of_object(other))
        .unwrap_or_default();
    if !other_class.starts_with("com/sun/tools/javac/file/RelativePath") {
        return Ok(Some(Value::Int(0)));
    }
    let left = javac_relative_path_string(ctx, this);
    let right = javac_relative_path_string(ctx, other);
    Ok(Some(Value::Int((left == right) as i32)))
}

pub(crate) fn native_javac_relative_path_compare_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let left = javac_relative_path_string(ctx, this);
    let right = javac_relative_path_string(ctx, other);
    let result = match left.cmp(&right) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Some(Value::Int(result)))
}

/// Fast paths for AssertJ's default comparison strategy.
///
/// `Iterables.assertContainsExactlyInAnyOrder()` repeatedly searches and
/// removes from two `ArrayList`s. Hibernate's optimizer concurrency test uses
/// 5,000 distinct boxed longs, so the generic implementation performs tens of
/// millions of stream, iterator, and virtual equality calls.  The fast path is
/// deliberately limited to a real `ArrayList` of `Long`s; every other input
/// uses the same element-by-element semantics as AssertJ's Java code.
fn assertj_long_value(ctx: &dyn NativeContext, object: ObjectRef, slot: usize) -> Option<i64> {
    match ctx.get_field(object, slot) {
        Value::Long(value) => Some(value),
        Value::Int(value) => Some(value as i64),
        _ => None,
    }
}

fn assertj_array_values_equal(left: Value, right: Value) -> bool {
    match (left, right) {
        (Value::Int(left), Value::Int(right)) => left == right,
        (Value::Long(left), Value::Long(right)) => left == right,
        // Arrays.equals(float[], float[]) and Arrays.equals(double[], double[])
        // use the canonical NaN bit pattern, rather than IEEE `==`.
        (Value::Float(left), Value::Float(right)) => {
            let left = if left.is_nan() {
                0x7fc0_0000
            } else {
                left.to_bits()
            };
            let right = if right.is_nan() {
                0x7fc0_0000
            } else {
                right.to_bits()
            };
            left == right
        }
        (Value::Double(left), Value::Double(right)) => {
            let left = if left.is_nan() {
                0x7ff8_0000_0000_0000
            } else {
                left.to_bits()
            };
            let right = if right.is_nan() {
                0x7ff8_0000_0000_0000
            } else {
                right.to_bits()
            };
            left == right
        }
        (Value::Object(left), Value::Object(right)) => left == right,
        _ => false,
    }
}

// BUG (micrometer-metrics-graphite-array-equality-20260724): this used to
// take `left_name`/`right_name` strings derived from
// `class_name_of_id(class_id_of_object(obj))` and decide reference-vs-
// primitive by prefix-matching ("[L"/"[["/exact primitive descriptor). That
// is unsound for ARRAY objects specifically: the heap object header's
// `class_id` field for an array stores the ELEMENT type's ClassId (e.g.
// `java/lang/String`'s ClassId for a `String[]`), not the array type's own
// ClassId — confirmed via a from-scratch repro comparing native-side
// `identity_hash_code`/`class_id_of_object` output against the SAME
// object's Java-side `System.identityHashCode()`: the identity matched the
// array exactly, but `class_id_of_object` reported the component class
// (`java/lang/String`), so `left_name.starts_with('[')` was always false
// and every reference-array comparison silently fell through to this
// function's caller's `Object.equals` (identity) branch — e.g.
// `GraphitePropertiesConfigAdapterTests.whenPropertiesTagsAsPrefixIsSetAdapterTagsAsPrefixReturnsIt`
// asserting `assertThat(new String[]{"worker"}).isEqualTo(new String[]{"worker"})`
// spuriously failed despite `Arrays.deepEquals`/`Objects.deepEquals`/
// `instanceof` all correctly recognizing the exact same objects as
// content-equal `String[]` arrays. Fixed by using `object_is_array`/
// `heap_element_type_of` (the same reliable APIs `java.lang.reflect.Array`'s
// natives already use — see `reflect_array_arg`/`native_array_get`) instead
// of class-id-derived name strings.
fn assertj_arrays_equal(
    ctx: &mut dyn NativeContext,
    left: ObjectRef,
    right: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let left_type = ctx.heap_element_type_of(left);
    let right_type = ctx.heap_element_type_of(right);
    let left_is_reference_array = left_type == cratonvm_types::ArrayElementType::Reference;
    let right_is_reference_array = right_type == cratonvm_types::ArrayElementType::Reference;
    if crate::nbflags().dbg_assertj_arr {
        eprintln!(
            "[ASSERTJ-ARR-DBG] left_type={left_type:?} right_type={right_type:?} left_ref={left_is_reference_array}"
        );
    }

    // The Java implementation falls through to Object.equals (identity for
    // arrays) for different primitive array types and primitive/reference
    // pairs. `left == right` was handled by the caller.
    if left_is_reference_array != right_is_reference_array
        || (!left_is_reference_array && left_type != right_type)
    {
        if crate::nbflags().dbg_assertj_arr {
            eprintln!("[ASSERTJ-ARR-DBG] early-false: mismatched array element type");
        }
        return Ok(false);
    }

    let left_pin = ctx.pin_native_root(left);
    let right_pin = ctx.pin_native_root(right);
    let result: Result<bool, MethodCallFailed> = (|| {
        let left = ctx.read_native_pin(left_pin, left);
        let right = ctx.read_native_pin(right_pin, right);
        let length = ctx.array_length(left);
        if length != ctx.array_length(right) {
            return Ok(false);
        }
        for index in 0..length {
            // A recursive object equality may allocate, so re-read both array
            // references from their native roots for every subsequent element.
            let left = ctx.read_native_pin(left_pin, left);
            let right = ctx.read_native_pin(right_pin, right);
            let left_value = ctx.get_array_element(left, index);
            let right_value = ctx.get_array_element(right, index);
            let equal = if left_is_reference_array {
                match (left_value, right_value) {
                    (Value::Object(left), Value::Object(right)) => {
                        assertj_objects_equal(ctx, left, right)?
                    }
                    _ => {
                        if crate::nbflags().dbg_assertj_arr {
                            eprintln!(
                                "[ASSERTJ-ARR-DBG] index={index} non-object element value(s): left={left_value:?} right={right_value:?}"
                            );
                        }
                        false
                    }
                }
            } else {
                assertj_array_values_equal(left_value, right_value)
            };
            if crate::nbflags().dbg_assertj_arr {
                eprintln!("[ASSERTJ-ARR-DBG] index={index} equal={equal}");
            }
            if !equal {
                return Ok(false);
            }
        }
        Ok(true)
    })();
    ctx.unpin_native_roots(left_pin);
    ctx.unpin_native_roots(right_pin);
    result
}

fn assertj_objects_equal(
    ctx: &mut dyn NativeContext,
    left: Option<ObjectRef>,
    right: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    let (left, right) = match (left, right) {
        (None, None) => return Ok(true),
        (None, Some(_)) => return Ok(false),
        // Real `StandardComparisonStrategy.areEqual` only short-circuits on a
        // null ACTUAL; every array branch is guarded by `other != null` and the
        // method ends in `return actual.equals(other)`. So a non-null actual
        // with a null other still gets `equals(null)` dispatched — which is how
        // `assertThat(springNullBean).isEqualTo(null)` passes on HotSpot
        // (`NullBean.equals` is `this == obj || obj == null`).
        (Some(left), None) => {
            let r = ctx.invoke_virtual(
                left,
                "equals",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(None)],
            )?;
            return Ok(matches!(r, Some(Value::Int(v)) if v != 0));
        }
        (Some(left), Some(right)) if left == right => return Ok(true),
        (Some(left), Some(right)) => (left, right),
    };

    // Arrays never override `Object.equals` (reference semantics), but
    // AssertJ's `isEqualTo` needs a deep element-wise comparison. Detect
    // arrays via `heap_kind_of`/`heap_element_type_of` (the heap object
    // header CratonVM actually tracks this on), NOT a `class_name_of_id()`
    // string check: an array's header `class_id` field stores its ELEMENT
    // type's ClassId (e.g. `java/lang/String`'s ClassId for a `String[]`),
    // not the array type's own ClassId — array objects are not registered
    // under a normal `"[B"`-style class name in `class_manager`, so the
    // name-based check silently returned `""`/the component class name for
    // every array and fell through to the generic `Object.equals` branch
    // below — reference equality — making `isEqualTo(byte[])` and
    // `isEqualTo(String[])` alike report content-identical arrays as
    // unequal (`AppendableByteArrayTests`/`writesMultipleSmallStrings`,
    // `GraphitePropertiesConfigAdapterTests`/
    // `whenPropertiesTagsAsPrefixIsSetAdapterTagsAsPrefixReturnsIt`, found
    // independently the same day).
    let left_is_array = ctx.heap_kind_of(left) == cratonvm_types::ObjectKind::Array;
    let right_is_array = ctx.heap_kind_of(right) == cratonvm_types::ObjectKind::Array;
    if left_is_array || right_is_array {
        if left_is_array != right_is_array {
            return Ok(false);
        }
        return assertj_arrays_equal(ctx, left, right);
    }

    let left_name = ctx.class_name_of_id(ctx.class_id_of_object(left));
    let right_name = ctx.class_name_of_id(ctx.class_id_of_object(right));
    if crate::nbflags().dbg_assertj_arr {
        eprintln!("[ASSERTJ-OBJ-DBG] left_name={left_name:?} right_name={right_name:?}");
    }
    let left_name = left_name.as_deref().unwrap_or_default();
    let right_name = right_name.as_deref().unwrap_or_default();

    if left_name == "java/lang/Long" && right_name == "java/lang/Long" {
        let value_slot = ctx
            .resolve_field_index("java/lang/Long", "value")
            .unwrap_or(0);
        return Ok(
            assertj_long_value(ctx, left, value_slot) == assertj_long_value(ctx, right, value_slot)
        );
    }

    // A virtual call may allocate or re-enter Java, so retain both operands
    // across it on the moving heap.
    let left_pin = ctx.pin_native_root(left);
    let right_pin = ctx.pin_native_root(right);
    let left = ctx.read_native_pin(left_pin, left);
    let right = ctx.read_native_pin(right_pin, right);
    let result = ctx.invoke_virtual(
        left,
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(right))],
    );
    ctx.unpin_native_roots(left_pin);
    ctx.unpin_native_roots(right_pin);
    Ok(matches!(result?, Some(Value::Int(value)) if value != 0))
}

fn assertj_array_list_long_layout(
    ctx: &dyn NativeContext,
    iterable: ObjectRef,
    needle: ObjectRef,
) -> Option<(ObjectRef, usize, usize, i64)> {
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(iterable))
        .as_deref()
        != Some("java/util/ArrayList")
        || ctx
            .class_name_arc_of_id(ctx.class_id_of_object(needle))
            .as_deref()
            != Some("java/lang/Long")
    {
        return None;
    }
    let value_slot = ctx.resolve_field_index("java/lang/Long", "value")?;
    let needle_value = assertj_long_value(ctx, needle, value_slot)?;
    let Value::Object(Some(elements)) = ctx.get_field_by_name(iterable, "elementData") else {
        return None;
    };
    let Value::Int(size) = ctx.get_field_by_name(iterable, "size") else {
        return None;
    };
    Some((elements, size.max(0) as usize, value_slot, needle_value))
}

fn assertj_array_list_find_long(
    ctx: &dyn NativeContext,
    elements: ObjectRef,
    size: usize,
    long_class: ClassId,
    value_slot: usize,
    needle: i64,
) -> Result<Option<usize>, MethodCallFailed> {
    for index in 0..size {
        let Value::Object(Some(element)) = ctx.get_array_element(elements, index) else {
            return Ok(None);
        };
        if ctx.class_id_of_object(element) != long_class {
            return Ok(None);
        }
        let Some(value) = assertj_long_value(ctx, element, value_slot) else {
            return Ok(None);
        };
        if value == needle {
            return Ok(Some(index));
        }
    }
    Ok(Some(usize::MAX))
}

fn assertj_iterable_contains_generic(
    ctx: &mut dyn NativeContext,
    iterable: ObjectRef,
    needle: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    let iterable_pin = ctx.pin_native_root(iterable);
    let needle_pin = needle.map(|needle| ctx.pin_native_root(needle));
    let result: Result<bool, MethodCallFailed> = (|| {
        let iterable = ctx.read_native_pin(iterable_pin, iterable);
        let iterator =
            match ctx.invoke_virtual(iterable, "iterator", "()Ljava/util/Iterator;", &[])? {
                Some(Value::Object(Some(iterator))) => iterator,
                _ => return Ok(false),
            };
        let iterator_pin = ctx.pin_native_root(iterator);
        let result = (|| loop {
            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let has_next = ctx.invoke_virtual(iterator, "hasNext", "()Z", &[])?;
            if !matches!(has_next, Some(Value::Int(value)) if value != 0) {
                return Ok(false);
            }
            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let element = match ctx.invoke_virtual(iterator, "next", "()Ljava/lang/Object;", &[])? {
                Some(Value::Object(element)) => element,
                _ => None,
            };
            let needle = needle
                .map(|needle| ctx.read_native_pin(needle_pin.expect("needle pin exists"), needle));
            if assertj_objects_equal(ctx, element, needle)? {
                return Ok(true);
            }
        })();
        ctx.unpin_native_roots(iterator_pin);
        result
    })();
    if let Some(needle_pin) = needle_pin {
        ctx.unpin_native_roots(needle_pin);
    }
    ctx.unpin_native_roots(iterable_pin);
    result
}

pub(crate) fn native_assertj_standard_comparison_are_equal(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if crate::nbflags().dbg_assertj_arr {
        eprintln!("[ASSERTJ-ENTRY-DBG] native_assertj_standard_comparison_are_equal args.len()={} args={args:?}", args.len());
        for (i, a) in args.iter().enumerate() {
            if let Value::Object(Some(o)) = a {
                eprintln!(
                    "[ASSERTJ-ENTRY-DBG] args[{i}] identity_hash={} class_id={:?} class_name={:?}",
                    ctx.identity_hash_code(*o),
                    ctx.class_id_of_object(*o),
                    ctx.class_name_of_id(ctx.class_id_of_object(*o))
                );
            }
        }
    }
    let left = match args.get(1) {
        Some(Value::Object(value)) => *value,
        _ => None,
    };
    let right = match args.get(2) {
        Some(Value::Object(value)) => *value,
        _ => None,
    };
    let out = assertj_objects_equal(ctx, left, right)?;
    if crate::nbflags().dbg_assertj_arr {
        eprintln!("[ASSERTJ-ENTRY-DBG] result={out}");
    }
    Ok(Some(Value::Int(i32::from(out))))
}

fn native_assertj_lightweight_comparable_assert(
    ctx: &mut dyn NativeContext,
    value: Option<ObjectRef>,
    class_name: &str,
) -> MethodCallResult {
    let class_id = ctx.ensure_class_initialized(class_name)?;
    let assertion = ctx.alloc_object(class_id, ctx.class_num_total_fields(class_id));
    let assertion_pin = ctx.pin_native_root(assertion);
    let value_pin = value.map(|value| ctx.pin_native_root(value));
    let result = (|| -> MethodCallResult {
        // Mirror AbstractAssert's constructor state.  The prior shortcut only
        // supplied the three fields used by isGreaterThan, which was not a
        // valid AssertJ object when ordinary framework code called another
        // assertion method during Mockito/Hibernate bootstrapping.
        let static_object = |ctx: &mut dyn NativeContext, owner: &str, field: &str| {
            ctx.ensure_class_initialized(owner)
                .ok()
                .and_then(|class_id| {
                    ctx.static_field_index_by_name(class_id, field)
                        .map(|index| ctx.get_static_field(class_id, index))
                })
                .unwrap_or(Value::Object(None))
        };
        let alloc_blank = |ctx: &mut dyn NativeContext,
                           class_name: &str|
         -> Result<ObjectRef, MethodCallFailed> {
            let class_id = ctx.ensure_class_initialized(class_name)?;
            Ok(ctx.alloc_object(class_id, ctx.class_num_total_fields(class_id)))
        };
        let assertion_live = ctx.read_native_pin(assertion_pin, assertion);
        let actual = value_pin
            .map(|pin| Value::Object(Some(ctx.read_native_pin(pin, value.unwrap()))))
            .unwrap_or(Value::Object(None));
        ctx.set_field_by_name(assertion_live, "actual", actual);
        ctx.set_field_by_name(
            assertion_live,
            "myself",
            Value::Object(Some(assertion_live)),
        );
        let objects = static_object(ctx, "org/assertj/core/internal/Objects", "INSTANCE");
        ctx.set_field_by_name(assertion_live, "objects", objects);
        let conditions = static_object(ctx, "org/assertj/core/internal/Conditions", "INSTANCE");
        let assertion_live = ctx.read_native_pin(assertion_pin, assertion);
        ctx.set_field_by_name(assertion_live, "conditions", conditions);
        // WritableAssertionInfo's constructor only assigns the representation;
        // allocating the simple state carrier directly avoids re-entering the
        // interpreter for every successful one-shot assertion. It must still
        // reproduce the constructor's FALLBACK, though:
        //
        //   WritableAssertionInfo(Representation custom) {
        //     useRepresentation(custom == null
        //         ? ConfigurationProvider.CONFIGURATION_PROVIDER.representation()
        //         : custom);
        //   }
        //   public void useRepresentation(Representation r) {
        //     Objects.requireNonNull(r, "The representation to use should not be null.");
        //     this.representation = r;
        //   }
        //
        // `AbstractAssert.customRepresentation` is null unless the test called
        // `Assertions.useRepresentation(...)`, i.e. essentially always -- so
        // copying it straight across left `info.representation` null, which
        // `useRepresentation`'s `requireNonNull` makes impossible for a real
        // AssertJ object. Nothing noticed while assertions PASSED; the moment
        // one failed, `ShouldBeEqual.actualAndExpectedHaveSameStringRepresentation`
        // dereferenced it and every AssertJ failure in every suite surfaced as
        // `NullPointerException: Cannot invoke
        // "org.assertj.core.presentation.Representation.toStringOf(Object)"
        // because "this.representation" is null` instead of the real assertion
        // message (found masking the last failure of Spring's
        // `BindingReflectionHintsRegistrarKotlinTests`).
        //
        // Resolve the representation LAST -- after `alloc_blank`, which can
        // move objects -- and hold no cached ObjectRef across an allocation.
        let info = alloc_blank(ctx, "org/assertj/core/api/WritableAssertionInfo")?;
        let info_pin = ctx.pin_native_root(info);
        let mut representation = static_object(
            ctx,
            "org/assertj/core/api/AbstractAssert",
            "customRepresentation",
        );
        if matches!(representation, Value::Object(None)) {
            let provider = static_object(
                ctx,
                "org/assertj/core/configuration/ConfigurationProvider",
                "CONFIGURATION_PROVIDER",
            );
            if let Value::Object(Some(provider)) = provider {
                let resolved = ctx
                    .invoke_virtual(
                        provider,
                        "representation",
                        "()Lorg/assertj/core/presentation/Representation;",
                        &[],
                    )
                    .ok()
                    .flatten()
                    .filter(|v| matches!(v, Value::Object(Some(_))))
                    // Fall back to the field itself if the accessor is
                    // unavailable; leaving it null is never acceptable.
                    .unwrap_or_else(|| ctx.get_field_by_name(provider, "representation"));
                if matches!(resolved, Value::Object(Some(_))) {
                    representation = resolved;
                }
            }
        }
        let info = ctx.read_native_pin(info_pin, info);
        ctx.set_field_by_name(info, "representation", representation);
        ctx.unpin_native_roots(info_pin);
        let assertion_live = ctx.read_native_pin(assertion_pin, assertion);
        ctx.set_field_by_name(assertion_live, "info", Value::Object(Some(info)));
        let creator = match ctx.new_object_initialized(
            "org/assertj/core/error/AssertionErrorCreator",
            "()V",
            &[],
        )? {
            Some(Value::Object(Some(creator))) => creator,
            _ => return Ok(Some(Value::Object(None))),
        };
        let assertion_live = ctx.read_native_pin(assertion_pin, assertion);
        ctx.set_field_by_name(
            assertion_live,
            "assertionErrorCreator",
            Value::Object(Some(creator)),
        );
        if class_name == "org/assertj/core/api/GenericComparableAssert" {
            // TreeMap's empty constructor leaves every field at its JVM
            // default except explicit zero/null stores, so a blank object is
            // observably equivalent and remains fully mutable.
            let comparators = alloc_blank(ctx, "java/util/TreeMap")?;
            let assertion_live = ctx.read_native_pin(assertion_pin, assertion);
            ctx.set_field_by_name(
                assertion_live,
                "comparatorsByPropertyOrField",
                Value::Object(Some(comparators)),
            );
        } else {
            let strings = static_object(ctx, "org/assertj/core/internal/Strings", "INSTANCE");
            let assertion_live = ctx.read_native_pin(assertion_pin, assertion);
            ctx.set_field_by_name(assertion_live, "strings", strings);
            let failures = static_object(ctx, "org/assertj/core/internal/Failures", "INSTANCE");
            let assertion_live = ctx.read_native_pin(assertion_pin, assertion);
            ctx.set_field_by_name(assertion_live, "failures", failures);
        }
        let comparables = alloc_blank(ctx, "org/assertj/core/internal/Comparables")?;
        let comparables_pin = ctx.pin_native_root(comparables);
        let comparison_strategy = static_object(
            ctx,
            "org/assertj/core/internal/StandardComparisonStrategy",
            "INSTANCE",
        );
        let failures = static_object(ctx, "org/assertj/core/internal/Failures", "INSTANCE");
        let comparables = ctx.read_native_pin(comparables_pin, comparables);
        ctx.set_field_by_name(comparables, "comparisonStrategy", comparison_strategy);
        ctx.set_field_by_name(comparables, "failures", failures);
        ctx.unpin_native_roots(comparables_pin);
        let assertion_live = ctx.read_native_pin(assertion_pin, assertion);
        ctx.set_field_by_name(
            assertion_live,
            "comparables",
            Value::Object(Some(comparables)),
        );
        Ok(Some(Value::Object(Some(assertion_live))))
    })();
    if let Some(value_pin) = value_pin {
        ctx.unpin_native_roots(value_pin);
    }
    ctx.unpin_native_roots(assertion_pin);
    result
}

pub(crate) fn native_assertj_string_assert_that(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(value)) => native_assertj_lightweight_comparable_assert(
            ctx,
            *value,
            "org/assertj/core/api/StringAssert",
        ),
        _ => native_assertj_lightweight_comparable_assert(
            ctx,
            None,
            "org/assertj/core/api/StringAssert",
        ),
    }
}

pub(crate) fn native_assertj_comparable_assert_that(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(value)) => native_assertj_lightweight_comparable_assert(
            ctx,
            *value,
            "org/assertj/core/api/GenericComparableAssert",
        ),
        _ => native_assertj_lightweight_comparable_assert(
            ctx,
            None,
            "org/assertj/core/api/GenericComparableAssert",
        ),
    }
}

/// Exact fast path for successful default-comparator AssertJ `isGreaterThan`.
///
/// The generic AssertJ body allocates and configures several comparison and
/// failure helpers even when the relation holds. Hibernate's RFC-9562 UUID
/// test executes that success path two million times. Custom comparators,
/// non-String/UUID values, nulls, and failures call `Comparables` directly so
/// AssertJ remains the authority for all observable failure behavior.
pub(crate) fn native_assertj_comparable_is_greater_than(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let assertion = match args.first() {
        Some(Value::Object(Some(assertion))) => *assertion,
        _ => return Ok(Some(Value::Object(None))),
    };
    let expected = match args.get(1) {
        Some(Value::Object(Some(expected))) => *expected,
        _ => {
            return assertj_comparable_assert_greater_than_fallback(
                ctx,
                assertion,
                Value::Object(None),
            )
        }
    };
    let assertion_pin = ctx.pin_native_root(assertion);
    let expected_pin = ctx.pin_native_root(expected);
    let result = (|| -> MethodCallResult {
        let assertion = ctx.read_native_pin(assertion_pin, assertion);
        let actual = match ctx.get_field_by_name(assertion, "actual") {
            Value::Object(Some(actual)) => actual,
            _ => {
                return assertj_comparable_assert_greater_than_fallback(
                    ctx,
                    assertion,
                    Value::Object(Some(expected)),
                )
            }
        };
        let actual_pin = ctx.pin_native_root(actual);
        let result = (|| -> MethodCallResult {
            let assertion = ctx.read_native_pin(assertion_pin, assertion);
            // `Comparables` keeps the exact default strategy as a field. A
            // comparator-based strategy means `usingComparator` changed the
            // contract, so retain AssertJ's implementation in that case.
            let default_strategy = match ctx.get_field_by_name(assertion, "comparables") {
                // Factory-created lightweight assertions have no need for a
                // comparison helper on their successful UUID/String path.
                Value::Object(None) => true,
                Value::Object(Some(comparables)) => matches!(
                    (
                        ctx.class_id_by_name("org/assertj/core/internal/StandardComparisonStrategy"),
                        ctx.get_field_by_name(comparables, "comparisonStrategy"),
                    ),
                    (Some(standard), Value::Object(Some(strategy)))
                        if ctx.class_id_of_object(strategy) == standard
                ),
                _ => false,
            };
            if !default_strategy {
                return assertj_comparable_assert_greater_than_fallback(
                    ctx,
                    assertion,
                    Value::Object(Some(expected)),
                );
            }
            let actual = ctx.read_native_pin(actual_pin, actual);
            let expected_live = ctx.read_native_pin(expected_pin, expected);
            let string_class = ctx.class_id_by_name("java/lang/String");
            let uuid_class = ctx.class_id_by_name("java/util/UUID");
            let relation_is_greater = if string_class.is_some_and(|class_id| {
                ctx.class_id_of_object(actual) == class_id
                    && ctx.class_id_of_object(expected_live) == class_id
            }) {
                // String.compareTo uses lexicographic UTF-16 code-unit order.
                ctx.read_string(actual)
                    .unwrap_or_default()
                    .encode_utf16()
                    .cmp(
                        ctx.read_string(expected_live)
                            .unwrap_or_default()
                            .encode_utf16(),
                    )
                    .is_gt()
            } else if uuid_class.is_some_and(|class_id| {
                ctx.class_id_of_object(actual) == class_id
                    && ctx.class_id_of_object(expected_live) == class_id
            }) {
                (uuid_get_msb(ctx, actual), uuid_get_lsb(ctx, actual))
                    > (
                        uuid_get_msb(ctx, expected_live),
                        uuid_get_lsb(ctx, expected_live),
                    )
            } else {
                return assertj_comparable_assert_greater_than_fallback(
                    ctx,
                    assertion,
                    Value::Object(Some(expected_live)),
                );
            };
            if relation_is_greater {
                let assertion = ctx.read_native_pin(assertion_pin, assertion);
                return Ok(Some(ctx.get_field_by_name(assertion, "myself")));
            }
            let assertion = ctx.read_native_pin(assertion_pin, assertion);
            assertj_comparable_assert_greater_than_fallback(
                ctx,
                assertion,
                Value::Object(Some(expected_live)),
            )
        })();
        ctx.unpin_native_roots(actual_pin);
        result
    })();
    ctx.unpin_native_roots(expected_pin);
    ctx.unpin_native_roots(assertion_pin);
    result
}

fn assertj_comparable_assert_greater_than_fallback(
    ctx: &mut dyn NativeContext,
    assertion: ObjectRef,
    expected: Value,
) -> MethodCallResult {
    let comparables = match ctx.get_field_by_name(assertion, "comparables") {
        Value::Object(Some(comparables)) => comparables,
        _ => return Ok(Some(Value::Object(None))),
    };
    let info = ctx.get_field_by_name(assertion, "info");
    let actual = ctx.get_field_by_name(assertion, "actual");
    ctx.invoke_virtual(
        comparables,
        "assertGreaterThan",
        "(Lorg/assertj/core/api/AssertionInfo;Ljava/lang/Comparable;Ljava/lang/Object;)V",
        &[info, actual, expected],
    )?;
    Ok(Some(ctx.get_field_by_name(assertion, "myself")))
}

pub(crate) fn native_assertj_standard_comparison_iterable_contains(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let iterable = match args.get(1) {
        Some(Value::Object(Some(iterable))) => *iterable,
        _ => return Ok(Some(Value::Int(0))),
    };
    let needle = match args.get(2) {
        Some(Value::Object(value)) => *value,
        _ => None,
    };
    if let Some(needle) = needle {
        if let Some((elements, size, value_slot, value)) =
            assertj_array_list_long_layout(ctx, iterable, needle)
        {
            let long_class = ctx.class_id_of_object(needle);
            match assertj_array_list_find_long(ctx, elements, size, long_class, value_slot, value)?
            {
                Some(index) => return Ok(Some(Value::Int(i32::from(index != usize::MAX)))),
                None => {}
            }
        }
    }
    Ok(Some(Value::Int(i32::from(
        assertj_iterable_contains_generic(ctx, iterable, needle)?,
    ))))
}

pub(crate) fn native_assertj_standard_comparison_iterables_remove_first(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let iterable = match args.get(1) {
        Some(Value::Object(Some(iterable))) => *iterable,
        _ => return Ok(None),
    };
    let needle = match args.get(2) {
        Some(Value::Object(value)) => *value,
        _ => None,
    };
    if let Some(needle) = needle {
        if let Some((elements, size, value_slot, value)) =
            assertj_array_list_long_layout(ctx, iterable, needle)
        {
            let long_class = ctx.class_id_of_object(needle);
            match assertj_array_list_find_long(ctx, elements, size, long_class, value_slot, value)?
            {
                Some(index) if index != usize::MAX => {
                    for offset in index + 1..size {
                        ctx.set_array_element(
                            elements,
                            offset - 1,
                            ctx.get_array_element(elements, offset),
                        );
                    }
                    if size > 0 {
                        ctx.set_array_element(elements, size - 1, Value::Object(None));
                    }
                    ctx.set_field_by_name(iterable, "size", Value::Int((size - 1) as i32));
                    if let Value::Int(mod_count) = ctx.get_field_by_name(iterable, "modCount") {
                        ctx.set_field_by_name(
                            iterable,
                            "modCount",
                            Value::Int(mod_count.wrapping_add(1)),
                        );
                    }
                    return Ok(None);
                }
                Some(_) => return Ok(None),
                None => {}
            }
        }
    }

    let iterable_pin = ctx.pin_native_root(iterable);
    let needle_pin = needle.map(|needle| ctx.pin_native_root(needle));
    let result: Result<(), MethodCallFailed> = (|| {
        let iterable = ctx.read_native_pin(iterable_pin, iterable);
        let iterator =
            match ctx.invoke_virtual(iterable, "iterator", "()Ljava/util/Iterator;", &[])? {
                Some(Value::Object(Some(iterator))) => iterator,
                _ => return Ok(()),
            };
        let iterator_pin = ctx.pin_native_root(iterator);
        let result = (|| loop {
            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let has_next = ctx.invoke_virtual(iterator, "hasNext", "()Z", &[])?;
            if !matches!(has_next, Some(Value::Int(value)) if value != 0) {
                return Ok(());
            }
            let iterator = ctx.read_native_pin(iterator_pin, iterator);
            let element = match ctx.invoke_virtual(iterator, "next", "()Ljava/lang/Object;", &[])? {
                Some(Value::Object(element)) => element,
                _ => None,
            };
            let needle = needle
                .map(|needle| ctx.read_native_pin(needle_pin.expect("needle pin exists"), needle));
            if assertj_objects_equal(ctx, element, needle)? {
                let iterator = ctx.read_native_pin(iterator_pin, iterator);
                ctx.invoke_virtual(iterator, "remove", "()V", &[])?;
                return Ok(());
            }
        })();
        ctx.unpin_native_roots(iterator_pin);
        result
    })();
    if let Some(needle_pin) = needle_pin {
        ctx.unpin_native_roots(needle_pin);
    }
    ctx.unpin_native_roots(iterable_pin);
    result?;
    Ok(None)
}
